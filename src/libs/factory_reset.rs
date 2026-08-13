//! Factory-reset phase 1: arm a durable ledger, then hand the device to a
//! reboot.
//!
//! The actual disk wipe ("phase 2") never runs from this process. It is a
//! separate, more privileged executor that meta-fiber installs and that reads
//! the ledger written here at the next boot (see the later tasks that ship
//! it). This module's entire job is the handoff to that executor: confirm it
//! is actually present before doing anything irreversible, write down what
//! was requested and by whom, and then reboot — in a way that can never leave
//! a reboot with no wipe behind it looking like success, and can never leave
//! an armed wipe with no reboot behind it waiting to trigger on the next
//! unrelated power cut.
//!
//! Dispatched from [`crate::libs::mqtt::monitor`]'s config-command executor
//! for `MqttCommand::FactoryReset`, the same way `RestartApplication` and
//! `PowerOffDevice` are.
//!
//! ## The log-prefix question
//!
//! `system_control::audit_and_flush` hardcodes a `[MQTT Monitor]` log prefix
//! internally; only the `label` argument (here, `"factory_reset"`) is
//! threaded through. Calling it from here means the WARN lines this module
//! triggers via that function read `[MQTT Monitor] WARN: ...` rather than,
//! say, `[factory_reset] WARN: ...`. That is accepted as-is rather than
//! changing `audit_and_flush`'s signature: the command genuinely does arrive
//! over MQTT like the reboot/power-off commands that function already serves,
//! so the prefix is not wrong, just less specific than it could be — and
//! composing the existing, already-reviewed function is preferable to
//! reopening Task 3's signature and tests for a cosmetic log difference. Log
//! lines this module prints directly (not through `audit_and_flush`) use
//! `[factory_reset]`.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::libs::mqtt::messages::PostResetAction;
use crate::libs::storage::StorageHandle;
use crate::libs::system_control;

/// Where the phase-1 ledger is written.
///
/// Inside `/data/fiber`, the one path this process's systemd sandbox
/// (`ReadWritePaths=/data/fiber`) can actually write to — the same
/// constraint documented on `lorawan::cluster::CLUSTER_STATE_DIR`. Phase 1
/// can only ever write here; anything outside this tree is phase 2's job,
/// running unsandboxed after the reboot this module triggers.
pub const LEDGER_PATH: &str = "/data/fiber/factory_reset_request.json";

/// Current on-disk shape of [`ResetRequest`]. Bump this whenever a field is
/// added, removed or changes meaning; [`ResetRequest::read`] refuses any
/// value it does not recognize rather than guess at an unfamiliar shape.
const CURRENT_SCHEMA_VERSION: u32 = 1;

/// The phase-2 executor binary this image must ship before a factory reset
/// can be anything more than an unrecoverable reboot. A later task owns
/// installing it at exactly this path via meta-fiber.
pub(crate) const PHASE2_EXECUTOR_BINARY: &str = "/usr/bin/fiber-factory-reset";

/// The systemd unit that runs [`PHASE2_EXECUTOR_BINARY`] at boot, reading
/// [`LEDGER_PATH`]. A later task owns shipping this unit. Until both this and
/// the binary above exist on an image, [`executor_installed`] must return
/// `false` — that is the correct, intended behavior today, not a bug to fix
/// in this change.
const PHASE2_EXECUTOR_UNIT: &str = "fiber-factory-reset.service";

/// How long [`request_factory_reset`] waits for the storage thread to
/// checkpoint and confirm before telling it to shut down. Bounded the same
/// way `system_control::TEARDOWN_AUDIT_FLUSH_TIMEOUT` is: a wedged storage
/// thread must not hold up a reboot that phase 2 is depending on, and the WAL
/// is crash-safe regardless of whether this checkpoint lands, so a timeout
/// here degrades to a WARN rather than aborting an already-armed reset.
const STORAGE_SHUTDOWN_TIMEOUT: std::time::Duration = system_control::TEARDOWN_AUDIT_FLUSH_TIMEOUT;

/// Serialize `value` as pretty JSON into `path`, atomically and durably:
/// tmp file -> write -> `sync_all` -> rename -> fsync the containing directory.
///
/// Every file this module writes — the phase-1 ledger, the phase-2 in-progress
/// marker, the phase-2 result — is read on the boot *after* a deliberate
/// reboot or an interrupted wipe, so a torn write must leave either the old
/// file or the new one and never half of one. One implementation rather than
/// three copies of the same five steps, mirroring
/// `power::standby::StandbyMarker::write`'s approach exactly.
fn write_json_durably<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let json =
        serde_json::to_vec_pretty(value).map_err(|e| format!("cannot serialize JSON: {e}"))?;

    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;

    let tmp_path_string = format!("{}.tmp", path.display());
    let tmp_path = Path::new(&tmp_path_string);
    {
        let mut f = fs::File::create(tmp_path)
            .map_err(|e| format!("cannot create {}: {e}", tmp_path.display()))?;
        f.write_all(&json)
            .map_err(|e| format!("cannot write {}: {e}", tmp_path.display()))?;
        f.sync_all()
            .map_err(|e| format!("cannot fsync {}: {e}", tmp_path.display()))?;
    }

    fs::rename(tmp_path, path).map_err(|e| {
        format!(
            "cannot rename {} to {}: {e}",
            tmp_path.display(),
            path.display()
        )
    })?;

    // The rename is only durable once the directory entry is. Without this,
    // the file can vanish on a power cut landing between the two — and both
    // the reboot phase 1 triggers and the wipe phase 2 performs are exactly
    // that kind of interruption.
    if let Ok(d) = fs::File::open(parent) {
        let _ = d.sync_all();
    }

    Ok(())
}

/// Read and validate a JSON file written by [`write_json_durably`].
///
/// Never fails loudly: a missing file, unparseable content, or a
/// `schema_version` this build does not recognize all degrade to `None` rather
/// than stopping whatever is booting from reading it. `label` only names the
/// file in the WARN lines.
fn read_json_checked<T: for<'de> Deserialize<'de>>(
    path: &Path,
    label: &str,
    schema_of: impl Fn(&T) -> u32,
) -> Option<T> {
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!(
                "[factory_reset] WARN: cannot read {} {}: {e}",
                label,
                path.display()
            );
            return None;
        }
    };

    let parsed: T = match serde_json::from_slice(&raw) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!(
                "[factory_reset] WARN: {} {} is unreadable ({e}) — treating as absent",
                label,
                path.display()
            );
            return None;
        }
    };

    let found = schema_of(&parsed);
    if found != CURRENT_SCHEMA_VERSION {
        eprintln!(
            "[factory_reset] WARN: {} {} has schema_version {} (expected {}) — treating as absent",
            label,
            path.display(),
            found,
            CURRENT_SCHEMA_VERSION
        );
        return None;
    }

    Some(parsed)
}

/// The durable, atomically-written record that a factory reset was armed.
///
/// Written by phase 1 (this module) before the reboot it triggers, and read
/// by the phase-2 executor at the next boot to know that a wipe was actually
/// requested — rather than this being an ordinary reboot — and what to do
/// once it finishes. Mirrors `power::standby::StandbyMarker`'s atomic write
/// pattern exactly: this file is read on the boot that follows the reboot it
/// causes, so a torn write must never leave a half-written ledger behind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetRequest {
    /// On-disk shape version. [`ResetRequest::read`] refuses anything but
    /// [`CURRENT_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Unix seconds the request was armed, for the on-device audit trail and
    /// `fiberctl`. An integer rather than a formatted string for the same
    /// reason as `StandbyMarker::entered_at_unix`: a wrong or unset system
    /// clock must not make the file unparseable.
    pub requested_at_unix: u64,
    /// The reason carried by the signed command. The authorization audit log
    /// it would otherwise live in is exactly what the wipe this command
    /// triggers is about to destroy, so this is the surviving copy.
    pub reason: String,
    /// Who signed the command. Kept for the same reason as `reason`.
    pub requested_by: String,
    /// What the device does once phase 2 finishes wiping: `Reboot` or
    /// `PowerOff`. Applied by phase 2 / the boot hook, never by this module —
    /// the reboot phase 1 triggers is always a plain `"reboot"` regardless of
    /// this value.
    pub post_action: PostResetAction,
    /// The original signed request's id — the same value
    /// `PendingChallenge::request_id` carried, threaded through
    /// `MqttCommand::FactoryReset` rather than minted here. A result reported
    /// after re-pairing (the wipe destroys the old pairing along with
    /// everything else) needs an id the server side already knows about to
    /// correlate against; a fresh, device-generated id would not mean
    /// anything to anyone who did not already have this file.
    pub request_id: String,
}

impl ResetRequest {
    /// Build a request stamped with the current wall clock.
    fn new(
        post_action: PostResetAction,
        reason: String,
        requested_by: String,
        request_id: String,
    ) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            requested_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            reason,
            requested_by,
            post_action,
            request_id,
        }
    }

    /// Write the ledger to `path`, atomically: tmp file -> write -> `sync_all`
    /// -> rename -> fsync the containing directory. Mirrors
    /// `power::standby::StandbyMarker::write`'s exact approach — this file is
    /// read on the boot that follows the reboot this triggers, so a torn
    /// write must leave either the old ledger or the new one, never half of
    /// one.
    pub fn write(&self, path: &Path) -> Result<(), String> {
        write_json_durably(path, self)
    }

    /// Read the ledger from `path`.
    ///
    /// Never panics: a missing file, unparseable content, or a
    /// `schema_version` this build does not recognize all degrade to `None`
    /// rather than stopping whatever is booting from reading it.
    pub fn read(path: &Path) -> Option<Self> {
        read_json_checked(path, "ledger", |r: &Self| r.schema_version)
    }

    /// Remove the ledger. Best-effort, same as `StandbyMarker::clear`: used
    /// by the phase-2 executor once it has finished, and by
    /// [`request_factory_reset`] to un-arm a reset whose reboot could not even
    /// be spawned.
    pub fn clear(path: &Path) {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("[factory_reset] WARN: cannot clear {}: {e}", path.display()),
        }
    }
}

/// Whether phase 2 is actually installed on this image: the executor binary
/// exists on disk AND systemd knows about the unit that runs it at boot AND
/// that unit is not masked and is actually wired to run (enabled, or static
/// and therefore only ever pulled in as a dependency).
///
/// This is the most important guard in the whole feature — a reboot with no
/// wipe behind it must never look like success — so all of these have to
/// hold, not just one. Fails closed by construction: until the executor and
/// its unit ship — enabled and unmasked — this returns `false` and
/// [`request_factory_reset`] refuses to arm anything.
fn executor_installed() -> bool {
    Path::new(PHASE2_EXECUTOR_BINARY).exists() && systemctl_unit_exists(PHASE2_EXECUTOR_UNIT)
}

/// Whether `unit` both exists (and isn't masked) and is actually wired to run
/// at boot.
///
/// Two separate `systemctl` calls are load-bearing here, not one:
///
/// * `systemctl cat <unit>` succeeds — exit 0 — for a **masked** unit too,
///   printing `# Unit <name> is masked.` instead of the unit file. Trusting
///   the exit code alone would let a masked (shipped-but-disabled) phase-2
///   unit pass the preflight, arm the ledger, and reboot into a wipe that
///   never runs while the signer is told SUCCESS — exactly the failure mode
///   this guard exists to prevent. So the stdout is inspected too, via
///   [`cat_output_reports_masked`].
/// * `systemctl is-enabled <unit>` catches the other way a unit can be
///   present and unmasked yet still not actually run: shipped but never
///   enabled (and not `static`, the one state where "not enabled" is
///   legitimate because something else pulls it in as a dependency).
///
/// Same "match on success/known-good text, don't trust the exact exit code
/// or wording beyond that" idiom as `lorawan::cluster::start_apply_unit`
/// (exit codes and stderr text are not contractual across systemd versions).
/// No `systemctl` at all — a container, a dev build with no systemd — must
/// fail closed rather than be treated as "found".
fn systemctl_unit_exists(unit: &str) -> bool {
    let cat_confirms_unmasked = match std::process::Command::new("systemctl")
        .args(["cat", unit])
        .output()
    {
        Ok(out) => {
            out.status.success()
                && !cat_output_reports_masked(&String::from_utf8_lossy(&out.stdout))
        }
        Err(_) => false,
    };
    if !cat_confirms_unmasked {
        return false;
    }

    matches!(
        systemctl_is_enabled(unit).as_deref(),
        Some("enabled") | Some("static")
    )
}

/// `systemctl cat` on a masked unit exits 0 and prints exactly
/// `# Unit <name> is masked.` in place of the unit file's contents — so a
/// masked unit must not be mistaken for a found one just because the command
/// succeeded. Split out as a pure function so the masked/unmasked shapes are
/// unit-testable without spawning a real `systemctl`.
fn cat_output_reports_masked(stdout: &str) -> bool {
    let stdout = stdout.trim_start();
    stdout.starts_with("# Unit") && stdout.contains("is masked")
}

/// The unit's enablement state per `systemctl is-enabled` (`"enabled"`,
/// `"static"`, `"masked"`, `"disabled"`, ...), or `None` if the command could
/// not even be run. `is-enabled` exits non-zero for most non-`enabled`
/// states, but the state text still lands on stdout either way, which is all
/// this reads — the exit code is not consulted.
fn systemctl_is_enabled(unit: &str) -> Option<String> {
    let out = std::process::Command::new("systemctl")
        .args(["is-enabled", unit])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Arm a factory reset and reboot into it.
///
/// Dispatched from `mqtt::monitor::execute_config_command` for
/// `MqttCommand::FactoryReset`, the same way `RestartApplication` and
/// `PowerOffDevice` are dispatched from the same match. This is phase 1 only
/// — it never touches user data itself:
///
/// 1. **Preflight.** Refuses outright if phase 2 is not installed on this
///    image. Arms nothing.
/// 2. **Audit.** Durably records who requested the wipe and why, before
///    anything else, because the log this would normally live in is what the
///    wipe destroys. Storage is still fully alive at this point and stays
///    that way until step 5, so every abort path below is guaranteed a live
///    writer behind it.
/// 3. **Arm the ledger.** Atomically, so the boot that follows never sees a
///    torn file. A failure here means nothing is armed and the device stays
///    up — audited as an abort, since storage hasn't been touched yet.
/// 4. **Reboot**, always with the `"reboot"` verb regardless of
///    `post_action` — the post-wipe power-off (if any) is applied on the
///    *next* boot by phase 2 / the boot hook, not by this call. If the reboot
///    itself cannot even be spawned, the ledger is cleared and the failure is
///    audited: an armed ledger with no reboot behind it is a landmine on the
///    next unrelated power cut.
/// 5. **Stop the storage writer** cleanly, best-effort, only after the
///    reboot has actually been spawned successfully. `spawn_teardown`
///    returns as soon as its thread exists; that thread still sleeps out
///    `system_control::TEARDOWN_GRACE` (1.5s) before the real `systemctl
///    reboot` runs, so stopping storage here still finishes comfortably
///    before the device goes down — while keeping storage available for
///    step 4's abort audit if the spawn itself fails.
///
/// **Why the return value matters.** `MqttCommand::FactoryReset` is *not* a
/// teardown command in `mqtt::monitor::is_teardown_command`, so the operator's
/// ack is built from this `Result` after the call returns: an `Err` from any of
/// steps 1, 3 or 4 — all of which leave the device up, connected and with
/// nothing erased — becomes an ERROR ack, and only a genuinely armed reset is
/// acked as SUCCESS. That is why every failure path above returns rather than
/// logging and carrying on. On the success path the return is prompt (step 5 is
/// bounded by [`STORAGE_SHUTDOWN_TIMEOUT`] and normally completes in
/// milliseconds, the queue having just been flushed in step 2), and the ack
/// reaches the socket the moment the `ConfigConfirm` branch hands control back
/// to `eventloop.poll()` — well inside the reboot thread's grace window.
pub fn request_factory_reset(
    post_action: PostResetAction,
    reason: String,
    requested_by: String,
    request_id: String,
    storage_handle: &Option<StorageHandle>,
) -> Result<(), String> {
    request_factory_reset_impl(
        post_action,
        reason,
        requested_by,
        request_id,
        storage_handle,
        Path::new(LEDGER_PATH),
        executor_installed,
        system_control::spawn_teardown,
    )
}

/// Testable core of [`request_factory_reset`].
///
/// The executor-presence check, the ledger path, and the reboot spawn are all
/// injected so unit tests can exercise the preflight-refusal and
/// abort-on-spawn-failure paths without touching a real systemd or actually
/// rebooting the machine running the test suite — `spawn_teardown` itself
/// finishes by running `systemctl reboot`, so it is deliberately not
/// unit-testable end to end (see `system_control`'s module docs).
#[allow(clippy::too_many_arguments)]
fn request_factory_reset_impl(
    post_action: PostResetAction,
    reason: String,
    requested_by: String,
    request_id: String,
    storage_handle: &Option<StorageHandle>,
    ledger_path: &Path,
    executor_check: impl Fn() -> bool,
    spawn_reboot: impl FnOnce(&'static str) -> Result<(), String>,
) -> Result<(), String> {
    // 1. Preflight — the most important guard in the whole feature. Missing
    // phase 2 means arm nothing.
    if !executor_check() {
        return Err(
            "factory reset executor not installed on this image — nothing was erased".to_string(),
        );
    }

    eprintln!(
        "[factory_reset] Factory reset requested by {}: {} (post_action={:?}, request_id={})",
        requested_by, reason, post_action, request_id
    );

    // 2. Durable audit row, before anything else: the only surviving record
    // of who requested the wipe and why, once the wipe destroys the
    // authorization log it would otherwise live in.
    system_control::audit_and_flush(
        "factory_reset",
        "FACTORY_RESET_REQUESTED",
        &reason,
        &requested_by,
        storage_handle,
    );

    // 3. Arm the ledger. A failure here leaves nothing armed and the device
    // stays up, same "must not silently proceed" contract as the preflight.
    // Storage has not been shut down at this point, so the abort audit below
    // has a live writer to land in.
    let request = ResetRequest::new(
        post_action,
        reason.clone(),
        requested_by.clone(),
        request_id,
    );
    if let Err(e) = request.write(ledger_path) {
        system_control::audit_and_flush(
            "factory_reset",
            "FACTORY_RESET_ABORTED",
            &reason,
            &requested_by,
            storage_handle,
        );
        return Err(format!(
            "cannot record factory reset request: {e} — nothing was erased"
        ));
    }

    // 4. Reboot. Always "reboot", never derived from `post_action`: the
    // post-wipe power-off is phase 2's problem on the next boot, not this
    // call's. Deliberately still before the storage shutdown below: storage
    // must stay alive until a reboot is actually in flight, or the one path
    // that most needs to be audited (a reboot that could not even be spawned)
    // would be racing a writer that had already been told to stop.
    if let Err(e) = spawn_reboot("reboot") {
        // An armed ledger with no reboot behind it is a landmine on the next
        // unrelated power cut: clear it immediately rather than leave a wipe
        // waiting to trigger on the wrong boot.
        ResetRequest::clear(ledger_path);
        system_control::audit_and_flush(
            "factory_reset",
            "FACTORY_RESET_ABORTED",
            &reason,
            &requested_by,
            storage_handle,
        );
        return Err(format!(
            "failed to reboot into factory reset: {e} — request aborted"
        ));
    }

    // 5. Stop the storage writer cleanly, now that the reboot is actually in
    // flight. `flush_sync` gives the bounded, confirmed wait for the
    // checkpoint; `shutdown()` afterwards tells the thread not to accept
    // anything further. The WAL is crash-safe regardless, so a timeout here
    // degrades to a WARN rather than undoing a reset that is already armed
    // and already rebooting.
    if let Some(storage) = storage_handle {
        if let Err(e) = storage.flush_sync(STORAGE_SHUTDOWN_TIMEOUT) {
            eprintln!("[factory_reset] WARN: storage did not confirm flush before shutdown: {e}");
        }
        if let Err(e) = storage.shutdown() {
            eprintln!("[factory_reset] WARN: failed to signal storage shutdown: {e}");
        }
    }

    Ok(())
}

// ============================ phase 2: the wipe engine ============================
//
// Everything below this line runs in the separate, unsandboxed
// `fiber-factory-reset` executor at early boot — never in `fiber_app`, which
// is confined to `ReadWritePaths=/data/fiber` and could not wipe the rest of
// `/data` even if it wanted to.
//
// Two rules hold for this whole section, and both are enforced by tests:
//
// * **No external programs.** Every destructive step is a `std::fs` call. A
//   shelled-out `rm -rf` on a device whose partition may not be mounted the
//   way we expect is exactly the failure mode this feature must not have —
//   and it would make the guardrails below trivially bypassable. See
//   [`ALLOWED_EXTERNAL_COMMANDS`].
// * **No hardcoded paths in the engine.** Every path the engine touches comes
//   from the [`ResetPlan`] it is handed, so tests point `root` at a `tempfile`
//   tempdir and can never reach a real `/data`. The production paths appear
//   exactly once, in [`ResetPlan::production`].

/// Delimits the phase-2 half of this file, so
/// `tests::phase_2_never_spawns_an_external_program` can assert against the
/// engine's own source without tripping over phase 1's legitimate `systemctl`
/// calls above.
#[allow(dead_code)]
const PHASE2_SECTION_MARKER: &str = "// ============================ phase 2: the wipe engine";

/// External programs phase 2 is allowed to run.
///
/// Deliberately, permanently empty. Kept as a named constant rather than a
/// comment so the intent is greppable and so the test that enforces it has
/// something to point at: if a future change ever needs a helper binary, this
/// list and the test guarding it have to be edited deliberately, in the same
/// diff, where a reviewer will see it.
#[allow(dead_code)]
const ALLOWED_EXTERNAL_COMMANDS: &[&str] = &[];

/// The partition phase 2 wipes in production. Its own mounted filesystem —
/// see [`MountGuard`].
pub const PRODUCTION_ROOT: &str = "/data";

/// Name of the durable "a wipe is mid-flight" marker, written under
/// [`ResetPlan::root`] before anything is removed and cleared only once the
/// result file is safely on disk.
///
/// Lives in [`ResetPlan::preserve`] for the obvious reason: the wipe must not
/// delete its own crash-recovery record halfway through.
pub const IN_PROGRESS_MARKER_NAME: &str = "factory_reset_in_progress.json";

/// Name of the result file, written under `<root>/fiber` — see
/// [`ResetPlan::result_path`] for why it is that subdirectory and not `root`
/// itself.
///
/// **Why it is not in the preserve list:** it is written *after* the wipe
/// pass, so the run that writes it cannot delete it, and a *later* reset
/// deleting it is correct — a stale result from a previous wipe must not
/// outlive the data it describes, and the later run writes its own. Preserving
/// it would be the more complicated option and would keep a dead record alive
/// across resets, so write-after-wipe it is.
pub const RESULT_FILE_NAME: &str = "factory_reset_result.json";

/// How many times a wipe may be attempted for one armed request before the
/// executor gives up and records `Failed`.
///
/// Each boot that finds an in-progress marker increments the attempt count in
/// it, so a wipe that reliably kills the device mid-way (a stuck kernel driver,
/// a dying eMMC) cannot turn into an unbootable reboot loop.
const MAX_WIPE_ATTEMPTS: u32 = 3;

/// How old an armed ledger may be before phase 2 refuses to act on it.
///
/// Phase 1 arms the ledger and reboots immediately, so a genuine request is
/// seconds old by the time the executor reads it. Anything a day old means the
/// reboot never happened, or the ledger outlived it somehow — and silently
/// wiping a device that has been monitoring patients since is far worse than
/// making an operator re-issue one signed command. Does not apply to a
/// half-finished wipe: see [`decide_boot_action`].
pub const STALE_LEDGER_AGE_SECS: u64 = 24 * 60 * 60;

/// How [`execute`] satisfies itself that [`ResetPlan::root`] really is the
/// separate data partition and not the rootfs.
///
/// `/data` is its own mount on a FIBER gateway. If it is *not* mounted when
/// the executor runs — a failed mount, a unit ordered before `data.mount` —
/// then `/data` is an ordinary empty directory on the rootfs, and a wipe there
/// would delete whatever happens to be sitting in it while reporting success
/// for a reset that erased none of the patient data it was supposed to.
/// Comparing `st_dev` is the cheapest honest test for "this is a different
/// filesystem from `/`".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountGuard {
    /// Refuse to wipe unless `root`'s `st_dev` differs from this path's.
    /// Production passes `/`.
    DistinctDeviceFrom(PathBuf),
    /// Skip the check.
    ///
    /// Test-only *by construction*: the variant does not exist outside
    /// `cfg(test)`, so the shipped `fiber-factory-reset` binary — which links
    /// this crate without `cfg(test)` — cannot name it, and
    /// [`ResetPlan::production`] could not opt out of the guard even by
    /// mistake. Needed because a `tempfile` root is on the same filesystem as
    /// `/` on many hosts, so an honest mount check can never pass in a unit
    /// test.
    #[cfg(test)]
    Unchecked,
}

/// Everything the wipe engine is allowed to touch, in one injectable value.
///
/// The engine reads all of its paths from here and hardcodes none, which is
/// what lets every test below run against a `tempfile` tempdir instead of a
/// real `/data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetPlan {
    /// The directory whose contents are wiped. `/data` in production.
    pub root: PathBuf,
    /// File/directory names directly under [`root`](Self::root) that survive.
    /// Matched exactly against `DirEntry::file_name`, so a name can never
    /// escape `root`.
    pub preserve: Vec<OsString>,
    /// Directories recreated under [`root`](Self::root) after the wipe, with
    /// the mode each is chmod'ed to. Relative, single-component paths.
    pub recreate: Vec<(PathBuf, u32)>,
    /// The "is `root` really its own partition?" check. See [`MountGuard`].
    pub mount_guard: MountGuard,
}

impl ResetPlan {
    /// The one and only place the production paths appear.
    ///
    /// * `qbee` — the fleet-management agent's state. Wiping it strands the
    ///   device: QBEE is how a reset gateway is reachable at all afterwards.
    /// * `lost+found` — ext4's own; removing it is never ours to do.
    /// * [`IN_PROGRESS_MARKER_NAME`] — this run's crash-recovery record.
    /// * `chirpstack` (0700) and `lorawan` (0755) — recreated because the
    ///   LoRaWAN stack refuses to start without them (a missing directory
    ///   shows up as systemd 226/NAMESPACE, not as a clear error), so a wiped
    ///   device would come back up with no LoRaWAN at all.
    pub fn production() -> Self {
        Self {
            root: PathBuf::from(PRODUCTION_ROOT),
            preserve: vec![
                OsString::from("qbee"),
                OsString::from("lost+found"),
                OsString::from(IN_PROGRESS_MARKER_NAME),
            ],
            recreate: vec![
                (PathBuf::from("chirpstack"), 0o700),
                (PathBuf::from("lorawan"), 0o755),
            ],
            mount_guard: MountGuard::DistinctDeviceFrom(PathBuf::from("/")),
        }
    }

    /// Where this plan's in-progress marker lives.
    pub fn marker_path(&self) -> PathBuf {
        self.root.join(IN_PROGRESS_MARKER_NAME)
    }

    /// Where this plan's result file lives: `<root>/fiber/<name>`, **not**
    /// `<root>/<name>`.
    ///
    /// The subdirectory is load-bearing and not a tidiness choice. Phase 2
    /// writes this file, but `fiber_app` is what reads it on the next boot and
    /// — crucially — what has to *delete* it once consumed
    /// ([`ResetOutcome::clear`], from `main.rs`'s boot hook). `fiber_app` runs
    /// under `ProtectSystem=strict` with `ReadWritePaths=/data/fiber`, so in
    /// its mount namespace `/data` itself is read-only: an unlink of
    /// `/data/factory_reset_result.json` fails with `EROFS`, and
    /// `ResetOutcome::clear` is best-effort (it only WARNs). A result file that
    /// can never be cleared is consumed on *every* subsequent boot — for
    /// `PostResetAction::PowerOff` that means the device re-enters standby
    /// forever and is unrecoverable without manual SSH/QBEE intervention, and
    /// for `Reboot` it means a duplicate `FACTORY_RESET_COMPLETED` audit row on
    /// every boot. Putting it inside the one directory that sandbox can write
    /// is what makes the consume-exactly-once contract actually hold.
    ///
    /// `/data/fiber` does not exist when phase 2 writes this (the wipe removed
    /// it; `fiber.service`'s `ExecStartPre` recreates it later), which is fine:
    /// [`write_json_durably`] `create_dir_all`s the parent first.
    ///
    /// The in-progress marker deliberately does *not* move here — see
    /// [`Self::marker_path`]: it must survive the wipe, which is exactly what
    /// `/data/fiber` does not do, and only the unsandboxed phase-2 executor
    /// ever writes or clears it.
    pub fn result_path(&self) -> PathBuf {
        self.root.join("fiber").join(RESULT_FILE_NAME)
    }

    fn is_preserved(&self, name: &OsStr) -> bool {
        self.preserve.iter().any(|p| p.as_os_str() == name)
    }
}

/// The durable record that a wipe started and has not finished.
///
/// Written before the first `remove_*` call and cleared only after the result
/// file is on disk, so its presence on a boot means exactly one thing: a wipe
/// was interrupted and still needs finishing. Carries the attempt count that
/// keeps a wipe which kills the device from becoming a reboot loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WipeAttempt {
    /// On-disk shape version, validated the same way the ledger's is.
    pub schema_version: u32,
    /// The armed request this wipe belongs to, copied from the ledger so a
    /// recovery run whose ledger has already been wiped away can still report
    /// against an id the server side knows.
    pub request_id: String,
    /// Unix seconds the first attempt began. Not refreshed on retries — the
    /// interesting number is how long the device has been mid-wipe.
    pub started_at_unix: u64,
    /// How many attempts have begun, including this one. Capped by
    /// [`MAX_WIPE_ATTEMPTS`].
    pub attempts: u32,
}

impl WipeAttempt {
    /// Write the marker, atomically. Errors are returned, not logged: a wipe
    /// whose attempt counter cannot be recorded must not start at all.
    pub fn write(&self, path: &Path) -> Result<(), String> {
        write_json_durably(path, self)
    }

    /// Read the marker, or `None` if there is nothing usable there.
    pub fn read(path: &Path) -> Option<Self> {
        read_json_checked(path, "in-progress marker", |m: &Self| m.schema_version)
    }

    /// Remove the marker. Best-effort — a leftover marker costs one extra,
    /// idempotent wipe pass on the next boot, never data.
    pub fn clear(path: &Path) {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!(
                "[factory_reset] WARN: cannot clear in-progress marker {}: {e}",
                path.display()
            ),
        }
    }

    /// The request to resume with when the marker outlived the ledger.
    ///
    /// A wipe that got as far as removing `/data/fiber` destroyed the ledger
    /// with it, so a power cut just after that leaves a marker and no ledger.
    /// The wipe still has to be finished — the recreate step has not run yet,
    /// and `chirpstack`/`lorawan` missing means no LoRaWAN — so the marker's
    /// `request_id` is enough to carry on with. `reason`/`requested_by` are
    /// genuinely gone; they are labelled as such rather than invented, and
    /// `post_action` falls back to `Reboot` because a device that comes back
    /// up can report what happened, while one that powers itself off cannot.
    pub fn recovered_request(&self) -> ResetRequest {
        ResetRequest {
            schema_version: CURRENT_SCHEMA_VERSION,
            requested_at_unix: self.started_at_unix,
            reason: "unknown — ledger was already wiped by an interrupted reset".to_string(),
            requested_by: "unknown".to_string(),
            post_action: PostResetAction::Reboot,
            request_id: self.request_id.clone(),
        }
    }
}

/// How a wipe ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetStatus {
    /// Everything outside the preserve set is gone and the recreate set is
    /// back.
    Completed,
    /// The wipe ran and the recreate set was applied, but at least one entry
    /// could not be removed or one directory could not be recreated. The
    /// per-path errors are in [`ResetOutcome::errors`].
    CompletedWithErrors,
    /// Nothing was wiped, or the wipe could not even be started: a guardrail
    /// refused, the attempt cap was reached, or the marker could not be
    /// written.
    Failed,
}

/// The durable record of what a wipe did, written under
/// [`RESULT_FILE_NAME`] before the in-progress marker is cleared.
///
/// Carries the ledger's `reason`/`requested_by`/`post_action`/`request_id`
/// forward because the wipe destroys the audit log those came from, and
/// because whatever reports the result after re-pairing needs an id the server
/// side already knows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetOutcome {
    /// On-disk shape version, validated the same way the ledger's is.
    pub schema_version: u32,
    pub status: ResetStatus,
    /// Unix seconds the wipe finished.
    pub finished_at_unix: u64,
    /// Which attempt produced this result (1-based); 0 when no attempt was
    /// ever started because a guardrail refused.
    pub attempts: u32,
    /// Entries directly under the root that were removed.
    pub removed: usize,
    /// Entries directly under the root that the preserve set kept.
    pub preserved: usize,
    /// Directories recreated, as absolute paths.
    pub recreated: Vec<String>,
    /// One `"<path>: <error>"` string per failure, or the guardrail's
    /// complaint when nothing was attempted.
    pub errors: Vec<String>,
    pub request_id: String,
    pub reason: String,
    pub requested_by: String,
    pub post_action: PostResetAction,
}

impl ResetOutcome {
    fn new(request: &ResetRequest, status: ResetStatus, attempts: u32) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            status,
            finished_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            attempts,
            removed: 0,
            preserved: 0,
            recreated: Vec::new(),
            errors: Vec::new(),
            request_id: request.request_id.clone(),
            reason: request.reason.clone(),
            requested_by: request.requested_by.clone(),
            post_action: request.post_action,
        }
    }

    /// Read a result file written by a previous phase-2 run. For whatever
    /// reports the outcome once the device is back up and re-paired.
    pub fn read(path: &Path) -> Option<Self> {
        read_json_checked(path, "result", |o: &Self| o.schema_version)
    }

    /// One-line summary for the journal.
    pub fn summary(&self) -> String {
        format!(
            "status={:?} attempt={} removed={} preserved={} recreated={} errors={}",
            self.status,
            self.attempts,
            self.removed,
            self.preserved,
            self.recreated.len(),
            self.errors.len()
        )
    }

    /// Remove the result file. Best-effort, same as [`ResetRequest::clear`]
    /// and [`WipeAttempt::clear`]: called once `main.rs`'s boot hook has
    /// folded this outcome into the fresh audit chain (and, for
    /// `PostResetAction::PowerOff`, requested standby), so a later, unrelated
    /// boot does not see the same result file and re-trigger on it.
    pub fn clear(path: &Path) {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!(
                "[factory_reset] WARN: cannot clear result file {}: {e}",
                path.display()
            ),
        }
    }

    /// Whether the result file at `path` is genuinely corrupt — bytes that
    /// were read successfully but do not parse as JSON, or parse but carry a
    /// `schema_version` this build does not recognize — as opposed to merely
    /// absent or transiently unreadable.
    ///
    /// This distinction exists specifically so `main.rs`'s boot hook can
    /// clear a truly corrupt result file (which can never become readable)
    /// without also clearing one that `fs::read` merely failed to read this
    /// once — a permission hiccup, a transient I/O error, anything other than
    /// `NotFound`. [`Self::read`] (via `read_json_checked`) deliberately
    /// collapses all of "absent", "unreadable", "unparseable" and "wrong
    /// schema" down to `None`, because that is the right thing for every
    /// other caller of `read` (booting must never fail loudly over a result
    /// file). But `main.rs` additionally wants to know when it is safe to
    /// *delete* the file outright, and doing that on a transient read error
    /// would permanently destroy the one on-disk record of a reset's
    /// outcome — silently skipping the `FACTORY_RESET_COMPLETED` audit row
    /// and, for `PostResetAction::PowerOff`, silently dropping the
    /// operator-requested standby entry, with no retry possible afterward.
    /// So this re-reads the file itself rather than reusing `read`'s
    /// collapsed `None`, and returns `false` (i.e. "leave it, try again next
    /// boot") for anything other than a definite parse/schema failure.
    pub fn is_corrupt(path: &Path) -> bool {
        let raw = match fs::read(path) {
            Ok(raw) => raw,
            // Absent, or `fs::read` itself failed (permissions, I/O glitch,
            // anything else): never treat as corrupt. A later boot gets
            // another chance to read it once whatever caused this passes.
            Err(_) => return false,
        };
        match serde_json::from_slice::<Self>(&raw) {
            Ok(parsed) => parsed.schema_version != CURRENT_SCHEMA_VERSION,
            Err(_) => true,
        }
    }
}

/// What `main.rs`'s boot hook should do about a phase-2 factory-reset result
/// file, decided purely from its content (or absence). Kept separate from the
/// `execute_standby`/audit-write side effects themselves — which need a live
/// storage handle and touch real standby state — so the decision itself is
/// unit-testable without either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostResetBootAction {
    /// No result file was found: an ordinary boot, nothing to do.
    None,
    /// A result file was found and must be consumed exactly once: fold it
    /// into the fresh audit chain as `FACTORY_RESET_COMPLETED`, then remove
    /// it. `enter_standby` is `true` for `PostResetAction::PowerOff` and
    /// `false` for `PostResetAction::Reboot` — the boot itself already *is*
    /// the requested reboot, so there is nothing further to do for it beyond
    /// consuming the file.
    Consume { enter_standby: bool },
}

/// Pure decision core of the boot hook in `main.rs`. See [`PostResetBootAction`].
///
/// Keys off `post_action` alone, deliberately ignoring `status`
/// (`Completed`/`CompletedWithErrors`/`Failed`): `post_action` is the
/// operator's requested end state for the device once the reset attempt is
/// over, decided when the reset was requested, not a function of how the
/// wipe itself went. Standby is trivially reversible — PoE reconnect or a
/// physical power cycle brings the device straight back up (see
/// `power::standby`) — so entering it is not, on its own, hiding a `Failed`
/// or `CompletedWithErrors` wipe: an operator who needs the device to stay
/// reachable to investigate a failed wipe can always bring it back with a
/// power cycle, same as after any other standby.
///
/// This does **not** mean the caller is free to describe the outcome as a
/// plain success once it decides to enter standby. The `FACTORY_RESET_COMPLETED`
/// audit row this decision leads to *does* carry the real `status`, but
/// nothing in this codebase exports the audit log or reports it anywhere
/// (there is no audit-export MQTT stream), and the result file this row was
/// built from is deleted right after — so the standby-entry reason string
/// `main.rs` hands to `execute_standby` (republished on the retained
/// `power/standby` topic and read by `fiberctl power`) is the *only* thing an
/// operator ever actually receives about this reset. That string must
/// therefore spell out `status` and the error count itself rather than
/// asserting the reset "completed" regardless of how it went — the rule
/// chosen here is "always enter standby when requested, but never describe a
/// `Failed`/`CompletedWithErrors` wipe as a success in the one message that
/// reaches anyone."
pub fn decide_post_reset_boot_action(outcome: Option<&ResetOutcome>) -> PostResetBootAction {
    match outcome {
        None => PostResetBootAction::None,
        Some(outcome) => PostResetBootAction::Consume {
            enter_standby: matches!(outcome.post_action, PostResetAction::PowerOff),
        },
    }
}

/// Refuse to wipe anything unless the plan's root is what it claims to be.
///
/// Checked before the in-progress marker is written and before any removal, so
/// a refusal leaves the filesystem exactly as it was found. Three separate
/// ways this can go wrong, all of which end with the wrong thing being
/// deleted:
///
/// * `root` does not exist, or is not a directory.
/// * `root` is a symlink. Following one to decide what to wipe means the
///   target of the link decides, not the plan.
/// * `root` is on the same filesystem as the [`MountGuard`]'s reference path
///   (`/` in production) — i.e. the data partition is not actually mounted, so
///   `root` is an ordinary rootfs directory. Wiping it would erase none of the
///   patient data the reset was asked to erase, while reporting success.
///
/// Plus the one preserve-set integrity check that matters: `root/qbee` must
/// not be a symlink. `qbee` is preserved by *name*, so a symlink there means
/// the real QBEE state lives under some other name inside the wipe area and
/// would be deleted — stranding the device with no way to manage it remotely.
fn check_guardrails(plan: &ResetPlan) -> Result<(), String> {
    let root_meta = fs::symlink_metadata(&plan.root)
        .map_err(|e| format!("cannot stat wipe root {}: {e}", plan.root.display()))?;

    if root_meta.file_type().is_symlink() {
        return Err(format!(
            "wipe root {} is a symlink — refusing to let its target decide what gets erased",
            plan.root.display()
        ));
    }
    if !root_meta.is_dir() {
        return Err(format!(
            "wipe root {} is not a directory",
            plan.root.display()
        ));
    }

    match &plan.mount_guard {
        MountGuard::DistinctDeviceFrom(reference) => {
            let reference_meta = fs::metadata(reference)
                .map_err(|e| format!("cannot stat {}: {e}", reference.display()))?;
            if root_meta.dev() == reference_meta.dev() {
                return Err(format!(
                    "{} is on the same filesystem as {} (st_dev {}) — the data partition is not \
                     mounted, so refusing to wipe",
                    plan.root.display(),
                    reference.display(),
                    root_meta.dev()
                ));
            }
        }
        #[cfg(test)]
        MountGuard::Unchecked => {}
    }

    // Every preserved name, not just `qbee`: the engine hardcodes no path, and
    // the reasoning applies to all of them equally.
    for name in &plan.preserve {
        let path = plan.root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "{} is a symlink — the preserve set matches by name, so the real data behind \
                     it would be wiped instead; refusing",
                    path.display()
                ));
            }
            // Absent is fine: a bench or pre-enrolment unit has no QBEE state
            // to keep, and no marker either on a first attempt.
            _ => {}
        }
    }

    Ok(())
}

/// What one wipe pass over the root did.
#[derive(Debug, Default)]
struct WipeTally {
    removed: usize,
    preserved: usize,
    errors: Vec<String>,
    /// The root itself could not be enumerated, so nothing was wiped at all.
    /// Distinct from per-entry errors because it means the wipe did not happen
    /// rather than happened imperfectly.
    unreadable_root: bool,
}

/// Remove everything directly under `plan.root` that the preserve set does not
/// name.
///
/// Exclude-list, not move-aside-then-restore: nothing is ever relocated, so a
/// power cut mid-pass can never leave preserved data parked somewhere the next
/// pass does not know to look. Symlinks are unlinked, never followed —
/// `symlink_metadata` decides the branch, so a symlink to a directory takes
/// the `remove_file` path and its target is untouched (`remove_dir_all` on a
/// symlink would be wrong twice over: it errors on modern std, and any
/// implementation that "worked" would be deleting someone else's tree).
///
/// Per-entry failures are collected and the loop continues. One immovable file
/// — an open descriptor, a bad block — must not stop the other twenty entries
/// from being erased on a device that was asked to erase them.
fn wipe_root(plan: &ResetPlan) -> WipeTally {
    let mut tally = WipeTally::default();

    let entries = match fs::read_dir(&plan.root) {
        Ok(entries) => entries,
        Err(e) => {
            tally.unreadable_root = true;
            tally
                .errors
                .push(format!("{}: cannot list: {e}", plan.root.display()));
            return tally;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tally
                    .errors
                    .push(format!("{}: cannot read entry: {e}", plan.root.display()));
                continue;
            }
        };

        let name = entry.file_name();
        if plan.is_preserved(&name) {
            tally.preserved += 1;
            continue;
        }

        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) => {
                tally
                    .errors
                    .push(format!("{}: cannot stat: {e}", path.display()));
                continue;
            }
        };

        // `is_dir()` here is on `symlink_metadata`, so it is false for a
        // symlink to a directory — which therefore gets unlinked below rather
        // than recursed into.
        let removed = if meta.file_type().is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };

        match removed {
            Ok(()) => tally.removed += 1,
            Err(e) => tally
                .errors
                .push(format!("{}: cannot remove: {e}", path.display())),
        }
    }

    tally
}

/// Recreate the plan's directories and chmod them to the requested modes.
///
/// Returns `(created_paths, errors)`. Called unconditionally, including after
/// a wipe that failed outright: a device missing `/data/lorawan` comes back up
/// with no LoRaWAN at all, and there is no version of "the wipe went badly"
/// that is improved by also breaking that.
fn recreate_dirs(plan: &ResetPlan) -> (Vec<String>, Vec<String>) {
    let mut created = Vec::new();
    let mut errors = Vec::new();

    for (relative, mode) in &plan.recreate {
        // The plan is trusted code, not input — but a `/etc` or a `../..`
        // slipping in here would chmod something outside the wipe root, so it
        // is refused rather than joined.
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            errors.push(format!(
                "{}: refusing to recreate a path that escapes the wipe root",
                relative.display()
            ));
            continue;
        }

        let path = plan.root.join(relative);
        if let Err(e) = fs::create_dir_all(&path) {
            errors.push(format!("{}: cannot create: {e}", path.display()));
            continue;
        }
        // `set_permissions` follows symlinks, and `create_dir_all` is happy
        // with a symlink that already points at a directory. Together that
        // would chmod the *target* — so if a symlink survived the wipe (a
        // failed removal, say) under one of these names, refuse rather than
        // reach through it.
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => {
                errors.push(format!(
                    "{}: exists but is not a real directory — refusing to chmod through it",
                    path.display()
                ));
                continue;
            }
            Err(e) => {
                errors.push(format!(
                    "{}: cannot stat after creating: {e}",
                    path.display()
                ));
                continue;
            }
        }
        // Separate from creation: `create_dir_all` applies the process umask,
        // so the mode has to be set explicitly afterwards or `chirpstack`
        // comes back world-readable.
        if let Err(e) = fs::set_permissions(&path, fs::Permissions::from_mode(*mode)) {
            errors.push(format!(
                "{}: cannot chmod to {:04o}: {e}",
                path.display(),
                mode
            ));
            continue;
        }
        created.push(path.display().to_string());
    }

    (created, errors)
}

/// Wipe the device, per `plan`, on behalf of the armed `request`.
///
/// The order of operations is the whole design, and every step of it exists
/// because of a specific way a wipe can go wrong:
///
/// 1. **Guardrails.** [`check_guardrails`] first, before anything is written
///    or removed, so a refusal leaves the filesystem byte-identical.
/// 2. **Attempt accounting.** The in-progress marker is read, its count
///    incremented, and the run refused once [`MAX_WIPE_ATTEMPTS`] is
///    exceeded — a wipe that kills the device must not become a reboot loop.
///    The recreate step still runs on that refusal.
/// 3. **Marker written, durably, before the first removal.** A power cut
///    anywhere in step 4 leaves a marker behind, which is what tells the next
///    boot to carry on. If the marker cannot be written, the wipe does not
///    start: an unaccounted wipe is worse than a refused one.
/// 4. **Wipe** by exclude-list, collecting per-entry errors instead of
///    aborting.
/// 5. **Recreate** unconditionally, errors or not.
/// 6. **Result file, then marker cleared** — in that order, never the
///    reverse. Clearing the marker first would make a power cut between the
///    two indistinguishable from "no reset was ever requested". If the result
///    cannot be written, the marker deliberately stays, and the next boot
///    retries (bounded by step 2).
///
/// Re-running this against a root that has already been wiped is harmless: the
/// pass finds nothing but preserved entries, and the recreate step is
/// idempotent.
///
/// ## What a refusal does and does not survive
///
/// A guardrail refusal (step 1) writes nothing: an existing in-progress marker
/// stays exactly as it was and the run does not count as an attempt. Nothing
/// here clears the ledger either — [`should_clear_ledger`] keeps it armed on a
/// `Failed` outcome, so the request is retried, and remains reportable, on the
/// next boot.
///
/// That covers the case where the refusal is about *this* root — `qbee` turned
/// into a symlink, say — because the ledger and marker are still readable.
///
/// **It does not cover a `/data` that never mounted**, and it is worth being
/// blunt about the limit: the ledger lives at [`LEDGER_PATH`], inside the very
/// partition that is missing. With `/data` unmounted, phase 1 wrote the ledger
/// into an empty rootfs directory shadowed by the real partition, so once the
/// mount comes back neither the ledger nor the marker is readable, and
/// [`decide_boot_action`] correctly reports [`BootAction::Nothing`]. The
/// request is then gone: the surviving evidence is whatever the journal kept
/// and the executor's non-zero exit. Retrying is bounded to the boots on which
/// `/data` is *still* unmounted — which is exactly the window in which the
/// ledger is readable again. Fixing that properly means phase 1 refusing to
/// arm a reset it cannot durably record on the data partition, which is not
/// this module's decision to make on its own; the executor's unit ordering
/// (`After=` the mount) is what keeps the case rare.
pub fn execute(plan: &ResetPlan, request: &ResetRequest) -> ResetOutcome {
    // 1. Guardrails. Nothing has been touched at this point and, if this
    // fails, nothing will be — not even the marker, which is why the refusal
    // is reported through the return value and the journal rather than
    // through a file written into a root we just decided we do not trust.
    if let Err(e) = check_guardrails(plan) {
        eprintln!("[factory_reset] ERROR: refusing to wipe: {e}");
        let mut outcome = ResetOutcome::new(request, ResetStatus::Failed, 0);
        outcome.errors.push(e);
        return outcome;
    }

    let marker_path = plan.marker_path();
    // Only a marker for *this* request continues its attempt count. A marker
    // left behind by an abandoned earlier reset must not make a freshly signed
    // one refuse before it starts — which is what would happen if the count
    // were inherited across request ids.
    let previous = WipeAttempt::read(&marker_path).filter(|m| m.request_id == request.request_id);
    let attempts = previous.as_ref().map_or(0, |m| m.attempts) + 1;

    // 2. Attempt cap.
    if attempts > MAX_WIPE_ATTEMPTS {
        eprintln!(
            "[factory_reset] ERROR: {} attempts already made for request {} — giving up",
            attempts - 1,
            request.request_id
        );
        let mut outcome = ResetOutcome::new(request, ResetStatus::Failed, attempts - 1);
        outcome.errors.push(format!(
            "wipe abandoned after {} attempts (cap {MAX_WIPE_ATTEMPTS})",
            attempts - 1
        ));
        // Still recreate: giving up on the wipe is no reason to also leave
        // LoRaWAN unable to start.
        let (created, errors) = recreate_dirs(plan);
        outcome.recreated = created;
        outcome.errors.extend(errors);
        // Result written, marker deliberately *kept*. Clearing it here would
        // reset the attempt count to zero and let the very next boot start the
        // whole wipe again — the reboot loop the cap exists to prevent. Kept,
        // this path is stable and idempotent: every subsequent boot re-reads
        // the same marker, refuses the same way, and re-applies the recreate
        // step. A newly signed reset carries a different `request_id` and is
        // therefore unaffected, per the filter above.
        write_result(plan, &outcome);
        return outcome;
    }

    // 3. Durable marker before the first removal.
    let marker = WipeAttempt {
        schema_version: CURRENT_SCHEMA_VERSION,
        request_id: request.request_id.clone(),
        started_at_unix: previous.as_ref().map_or_else(
            || {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            },
            |m| m.started_at_unix,
        ),
        attempts,
    };
    if let Err(e) = marker.write(&marker_path) {
        eprintln!("[factory_reset] ERROR: cannot record wipe attempt: {e} — not starting the wipe");
        let mut outcome = ResetOutcome::new(request, ResetStatus::Failed, 0);
        outcome.errors.push(e);
        // Deliberately *not* recreating here, unlike the attempt-cap path
        // above. That path has already proven the root writable and has
        // decided to stop wiping; this one has proven nothing except that the
        // root would not take a small JSON file, so the honest report is "the
        // tree was left exactly as found" rather than a half-applied recreate
        // on a root we cannot write to anyway.
        return outcome;
    }

    eprintln!(
        "[factory_reset] Wiping {} (attempt {attempts}/{MAX_WIPE_ATTEMPTS}, request {})",
        plan.root.display(),
        request.request_id
    );

    // 4. The wipe.
    let tally = wipe_root(plan);

    // 5. Recreate, unconditionally.
    let (created, recreate_errors) = recreate_dirs(plan);

    let mut outcome = ResetOutcome::new(
        request,
        if tally.unreadable_root {
            ResetStatus::Failed
        } else if tally.errors.is_empty() && recreate_errors.is_empty() {
            ResetStatus::Completed
        } else {
            ResetStatus::CompletedWithErrors
        },
        attempts,
    );
    outcome.removed = tally.removed;
    outcome.preserved = tally.preserved;
    outcome.errors = tally.errors;
    outcome.errors.extend(recreate_errors);
    outcome.recreated = created;

    for error in &outcome.errors {
        eprintln!("[factory_reset] WARN: {error}");
    }
    eprintln!("[factory_reset] Wipe finished: {}", outcome.summary());

    // 6. Result file first, and only if it is durably on disk does the marker
    // go. Clearing the marker first would make a power cut between the two
    // indistinguishable from "no reset was ever requested"; leaving it means
    // the next boot retries, bounded by step 2.
    if write_result(plan, &outcome) {
        WipeAttempt::clear(&plan.marker_path());
    } else {
        eprintln!("[factory_reset] WARN: keeping the in-progress marker so the next boot retries");
    }
    outcome
}

/// Write the result file. Returns whether it is durably on disk — the caller
/// decides what that means for the in-progress marker, because the two
/// `Failed` paths and the ordinary one want different things from it.
fn write_result(plan: &ResetPlan, outcome: &ResetOutcome) -> bool {
    let result_path = plan.result_path();
    match write_json_durably(&result_path, outcome) {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "[factory_reset] WARN: cannot write result {}: {e}",
                result_path.display()
            );
            false
        }
    }
}

/// Whether the executor should clear the phase-1 ledger after a run.
///
/// Only a wipe that actually happened un-arms the request. This is the whole
/// of Important #1 from review, in one testable place rather than as an
/// unconditional call in `main`: clearing the ledger on a `Failed` outcome
/// meant a guardrail refusal both erased the only durable evidence that an
/// operator ever signed a destructive command *and* guaranteed it would never
/// be retried — a signed reset that was neither performed nor recorded.
///
/// `CompletedWithErrors` does clear it. The wipe ran; some entries survived,
/// and that is what the result file is for. Re-running would not improve
/// matters, and the ledger normally went with `/data/fiber` anyway.
pub fn should_clear_ledger(status: ResetStatus) -> bool {
    match status {
        ResetStatus::Completed | ResetStatus::CompletedWithErrors => true,
        ResetStatus::Failed => false,
    }
}

/// What the executor should do with the state it found on disk.
#[derive(Debug, Clone, PartialEq)]
pub enum BootAction {
    /// No reset was requested and none was interrupted. The overwhelmingly
    /// common case on every ordinary boot.
    Nothing,
    /// A ledger was found but is older than [`STALE_LEDGER_AGE_SECS`] and no
    /// wipe was ever started. Clear it, wipe nothing.
    StaleRequest(ResetRequest),
    /// Wipe, on behalf of this request.
    Wipe(ResetRequest),
}

/// Decide what to do from the two files on disk, without touching either.
///
/// Pure, so the whole table is unit-testable. The two non-obvious rows:
///
/// * **Marker but no ledger → wipe anyway.** A wipe that reached
///   `/data/fiber` destroyed the ledger with it. A power cut just after that
///   leaves exactly this state, and the run still has unfinished business —
///   the recreate step has not happened, so `chirpstack`/`lorawan` are
///   missing and LoRaWAN cannot start. The marker's `request_id` carries the
///   request forward; see [`WipeAttempt::recovered_request`].
/// * **Marker present → staleness does not apply.** A half-wiped device that
///   spent a month in a drawer still must not be left half-wiped. Age only
///   ever vetoes a wipe that has not begun.
pub fn decide_boot_action(
    ledger: Option<ResetRequest>,
    marker: Option<WipeAttempt>,
    now_unix: u64,
) -> BootAction {
    match (ledger, marker) {
        (None, None) => BootAction::Nothing,
        (None, Some(marker)) => BootAction::Wipe(marker.recovered_request()),
        (Some(request), Some(_)) => BootAction::Wipe(request),
        (Some(request), None) => {
            // `saturating_sub` deliberately: the RTC is synced by a separate
            // oneshot unit, so at early boot the clock can easily read
            // *earlier* than the ledger was written. That must look like "not
            // stale" rather than wrap around into a huge age and silently
            // discard a genuine request.
            if now_unix.saturating_sub(request.requested_at_unix) > STALE_LEDGER_AGE_SECS {
                BootAction::StaleRequest(request)
            } else {
                BootAction::Wipe(request)
            }
        }
    }
}

/// What a real run would do, for `--dry-run`.
#[derive(Debug, Clone, PartialEq)]
pub struct DryRunReport {
    pub root: PathBuf,
    /// The guardrails' verdict. `Err` means a real run would refuse.
    pub guardrails: Result<(), String>,
    pub would_remove: Vec<PathBuf>,
    pub would_preserve: Vec<PathBuf>,
    pub would_recreate: Vec<(PathBuf, u32)>,
    /// Problems enumerating the root, if any.
    pub errors: Vec<String>,
}

impl DryRunReport {
    /// Human-readable rendering for the CLI.
    pub fn render(&self) -> String {
        let mut out = format!("factory reset dry run — root {}\n", self.root.display());
        match &self.guardrails {
            Ok(()) => out.push_str("guardrails: OK\n"),
            Err(e) => out.push_str(&format!("guardrails: WOULD REFUSE — {e}\n")),
        }
        out.push_str(&format!("would remove ({}):\n", self.would_remove.len()));
        for p in &self.would_remove {
            out.push_str(&format!("  - {}\n", p.display()));
        }
        out.push_str(&format!(
            "would preserve ({}):\n",
            self.would_preserve.len()
        ));
        for p in &self.would_preserve {
            out.push_str(&format!("  = {}\n", p.display()));
        }
        out.push_str(&format!(
            "would recreate ({}):\n",
            self.would_recreate.len()
        ));
        for (p, mode) in &self.would_recreate {
            out.push_str(&format!("  + {} mode {:04o}\n", p.display(), mode));
        }
        if !self.errors.is_empty() {
            out.push_str("problems:\n");
            for e in &self.errors {
                out.push_str(&format!("  ! {e}\n"));
            }
        }
        out
    }
}

/// Enumerate what [`execute`] would do, touching nothing.
///
/// Read-only by construction: `read_dir` and `symlink_metadata` only, no
/// removals, no marker, no result file. This is what makes it safe to validate
/// the plan on a bench unit — or on a live one — without erasing anything.
pub fn dry_run(plan: &ResetPlan) -> DryRunReport {
    let mut report = DryRunReport {
        root: plan.root.clone(),
        guardrails: check_guardrails(plan),
        would_remove: Vec::new(),
        would_preserve: Vec::new(),
        would_recreate: plan
            .recreate
            .iter()
            .map(|(rel, mode)| (plan.root.join(rel), *mode))
            .collect(),
        errors: Vec::new(),
    };

    match fs::read_dir(&plan.root) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => {
                        if plan.is_preserved(&entry.file_name()) {
                            report.would_preserve.push(entry.path());
                        } else {
                            report.would_remove.push(entry.path());
                        }
                    }
                    Err(e) => report
                        .errors
                        .push(format!("{}: cannot read entry: {e}", plan.root.display())),
                }
            }
        }
        Err(e) => report
            .errors
            .push(format!("{}: cannot list: {e}", plan.root.display())),
    }

    report.would_remove.sort();
    report.would_preserve.sort();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn sample() -> ResetRequest {
        ResetRequest::new(
            PostResetAction::Reboot,
            "unit test".to_string(),
            "dr.jane".to_string(),
            "req-1".to_string(),
        )
    }

    #[test]
    fn ledger_round_trips_through_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("factory_reset_request.json");
        let req = sample();

        req.write(&path).expect("write should succeed");
        let read_back = ResetRequest::read(&path).expect("ledger should be readable");

        assert_eq!(read_back, req);
    }

    #[test]
    fn read_returns_none_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        assert!(ResetRequest::read(&path).is_none());
    }

    #[test]
    fn read_returns_none_for_unparseable_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("factory_reset_request.json");
        fs::write(&path, b"not json at all").unwrap();
        assert!(ResetRequest::read(&path).is_none());
    }

    #[test]
    fn read_returns_none_for_an_unknown_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("factory_reset_request.json");
        let mut req = sample();
        req.schema_version = CURRENT_SCHEMA_VERSION + 1;
        req.write(&path).unwrap();

        assert!(
            ResetRequest::read(&path).is_none(),
            "a future/unknown schema_version must not be guessed at"
        );
    }

    #[test]
    fn clear_removes_the_ledger_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("factory_reset_request.json");
        sample().write(&path).unwrap();
        assert!(path.exists());

        ResetRequest::clear(&path);
        assert!(!path.exists());

        // Clearing again (nothing there) must not panic or error.
        ResetRequest::clear(&path);
    }

    /// The whole point of this guard: until a later task ships the phase-2
    /// binary and unit, this must be `false`. If this test ever starts
    /// failing because a build/test host happens to have a matching path and
    /// unit installed, that is real signal the guard needs a second look —
    /// not a reason to relax it.
    #[test]
    fn real_executor_check_fails_closed_before_phase_2_ships() {
        assert!(
            !executor_installed(),
            "phase-2 executor must not be detected as installed on this build/test host"
        );
    }

    #[test]
    fn systemctl_unit_exists_is_false_for_a_unit_that_does_not_exist() {
        assert!(!systemctl_unit_exists(
            "this-unit-almost-certainly-does-not-exist-anywhere.service"
        ));
    }

    /// The exact wording `systemctl cat` prints for a masked unit — verified
    /// against a real masked unit on a live systemd host. `cat` still exits 0
    /// in this case, so trusting the exit code alone would let a masked
    /// phase-2 unit pass the preflight.
    #[test]
    fn cat_output_reports_masked_detects_the_real_masked_wording() {
        assert!(cat_output_reports_masked("# Unit gdm.service is masked.\n"));
    }

    #[test]
    fn cat_output_reports_masked_is_false_for_an_ordinary_unit_file() {
        let unit_file = "# /usr/lib/systemd/system/fiber.service\n[Unit]\nDescription=FIBER agent\n[Service]\nExecStart=/usr/bin/fiber_app\n";
        assert!(!cat_output_reports_masked(unit_file));
    }

    #[test]
    fn cat_output_reports_masked_is_false_for_empty_output() {
        assert!(!cat_output_reports_masked(""));
    }

    #[test]
    fn preflight_missing_executor_refuses_without_writing_a_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("factory_reset_request.json");
        let spawned = Cell::new(false);

        let result = request_factory_reset_impl(
            PostResetAction::Reboot,
            "test".to_string(),
            "dr.jane".to_string(),
            "req-1".to_string(),
            &None,
            &path,
            || false,
            |_verb| {
                spawned.set(true);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("not installed"),
            "error should name the missing executor"
        );
        assert!(!path.exists(), "preflight failure must not arm a ledger");
        assert!(
            !spawned.get(),
            "preflight failure must not attempt a reboot"
        );
    }

    #[test]
    fn happy_path_arms_the_ledger_with_the_real_request_id_and_reboots_with_the_reboot_verb() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("factory_reset_request.json");
        let verb_seen: Cell<Option<&'static str>> = Cell::new(None);

        // post_action is PowerOff here specifically to prove the reboot verb
        // does not follow it.
        let result = request_factory_reset_impl(
            PostResetAction::PowerOff,
            "decommissioning".to_string(),
            "dr.jane".to_string(),
            "challenge-request-id-42".to_string(),
            &None,
            &path,
            || true,
            |verb| {
                verb_seen.set(Some(verb));
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert_eq!(
            verb_seen.get(),
            Some("reboot"),
            "the reboot verb must always be \"reboot\", regardless of post_action"
        );

        let ledger = ResetRequest::read(&path).expect("ledger should be armed");
        assert_eq!(ledger.reason, "decommissioning");
        assert_eq!(ledger.requested_by, "dr.jane");
        assert_eq!(ledger.post_action, PostResetAction::PowerOff);
        // Must be the id the caller passed in (the original signed request's
        // id), never a freshly minted one the server side has no way to
        // correlate against.
        assert_eq!(ledger.request_id, "challenge-request-id-42");
    }

    #[test]
    fn spawn_failure_clears_the_ledger_and_reports_the_abort() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("factory_reset_request.json");

        let result = request_factory_reset_impl(
            PostResetAction::Reboot,
            "test".to_string(),
            "dr.jane".to_string(),
            "req-1".to_string(),
            &None,
            &path,
            || true,
            |_verb| Err("thread pool exhausted".to_string()),
        );

        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("aborted"),
            "error should make clear the request did not stick"
        );
        assert!(
            !path.exists(),
            "an armed ledger with no reboot behind it must not survive a spawn failure"
        );
    }

    /// Regression test for the ordering bug: an earlier version of this
    /// function shut storage down BEFORE attempting the reboot spawn, so a
    /// spawn failure tried to audit `FACTORY_RESET_ABORTED` into a writer
    /// that had already been told to stop — the one abort path most in need
    /// of a surviving record could never actually produce one. Uses a real
    /// `StorageThread` (not `&None`) specifically so the audit row's survival
    /// is checked against a real database, not just a "no panic" assertion.
    #[test]
    fn spawn_failure_audits_the_abort_because_storage_is_still_alive() {
        use crate::libs::storage::{db::Database, StorageThread};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap().to_string();
        let (handle, join) = StorageThread::spawn(&db_path, 1).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("factory_reset_request.json");

        let result = request_factory_reset_impl(
            PostResetAction::Reboot,
            "test".to_string(),
            "dr.jane".to_string(),
            "req-1".to_string(),
            &Some(handle.clone()),
            &path,
            || true,
            |_verb| Err("thread pool exhausted".to_string()),
        );

        assert!(result.is_err());
        assert!(!path.exists());

        let conn = Database::new(&db_path, 1).unwrap().connect().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_log WHERE operation = 'FACTORY_RESET_ABORTED'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "FACTORY_RESET_ABORTED must actually persist now that storage isn't shut down \
             before the abort path runs"
        );

        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    /// Same ordering guarantee, exercised through the ledger-write-failure
    /// abort path rather than the spawn-failure one: storage is untouched at
    /// that point regardless, but this pins the behavior explicitly so a
    /// future reordering of steps 3/4/5 cannot silently drop it.
    #[test]
    fn ledger_write_failure_also_audits_the_abort() {
        use crate::libs::storage::{db::Database, StorageThread};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp.path().to_str().unwrap().to_string();
        let (handle, join) = StorageThread::spawn(&db_path, 1).unwrap();

        let dir = tempfile::tempdir().unwrap();
        // A file where the ledger's parent directory needs to be makes
        // `fs::create_dir_all` fail deterministically, without relying on
        // filesystem permissions.
        let blocker = dir.path().join("blocks-create-dir-all");
        fs::write(&blocker, b"not a directory").unwrap();
        let path = blocker.join("factory_reset_request.json");

        let spawned = Cell::new(false);
        let result = request_factory_reset_impl(
            PostResetAction::Reboot,
            "test".to_string(),
            "dr.jane".to_string(),
            "req-1".to_string(),
            &Some(handle.clone()),
            &path,
            || true,
            |_verb| {
                spawned.set(true);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(
            !spawned.get(),
            "a ledger write failure must not proceed to reboot"
        );

        let conn = Database::new(&db_path, 1).unwrap().connect().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_log WHERE operation = 'FACTORY_RESET_ABORTED'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "a ledger write failure must also audit an abort");

        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    // ===================== phase 2: the wipe engine =====================
    //
    // Every test here runs against a `tempfile` tempdir. Nothing in this
    // module reads a hardcoded path except `ResetPlan::production`, which is
    // asserted against (never executed) by
    // `production_plan_matches_what_meta_fiber_installs`.

    /// A plan shaped exactly like production's, pointed at `root`.
    ///
    /// `MountGuard::Unchecked` because a tempdir is on the same filesystem as
    /// `/` on many hosts, so the real check could never pass here. The check
    /// itself is exercised by `mount_guard_*` below.
    fn test_plan(root: &Path) -> ResetPlan {
        let production = ResetPlan::production();
        ResetPlan {
            root: root.to_path_buf(),
            preserve: production.preserve,
            recreate: production.recreate,
            mount_guard: MountGuard::Unchecked,
        }
    }

    /// Env var that lets a root test run acknowledge, explicitly, that the
    /// tests which cannot work without unprivileged file permissions are being
    /// skipped. See [`require_unprivileged`].
    const ROOT_SKIP_OPT_IN: &str = "FIBER_TEST_ALLOW_ROOT_SKIPS";

    /// Gate for the tests whose property is only observable as a normal user:
    /// root holds `CAP_DAC_OVERRIDE`, so no mode bits can deny it.
    ///
    /// Returns `false` (skip) only when the run has *declared* that it is
    /// happy with weaker coverage. Otherwise it **fails**, rather than
    /// returning early behind an `eprintln!` the harness swallows on a green
    /// run: this repo's CI runs as root inside `rust:1.94-bookworm`, and a
    /// safety test that quietly stops testing anything there is worse than one
    /// that is honestly absent.
    ///
    /// Two tests need it, and CI (which sets `FIBER_TEST_ALLOW_ROOT_SKIPS=1`
    /// — see `.gitlab/ci/mr_checks.yml`) therefore covers **neither**:
    ///
    /// * `an_entry_that_cannot_be_removed_still_leaves_the_recreated_dirs_behind`
    ///   — a removal denied by mode bits. Partly substituted in CI by
    ///   `a_recreate_error_alone_degrades_the_status_and_still_creates_the_rest`,
    ///   which reaches the same status/result/recreate contract through an
    ///   injection root cannot bypass.
    /// * `is_corrupt_is_false_for_a_file_that_exists_but_cannot_be_read`
    ///   — a present-but-unreadable result file. No substitute exists: a
    ///   transient `fs::read` failure can only be simulated with mode bits,
    ///   so this property is genuinely untested under CI. Only a local run as
    ///   an ordinary user exercises it.
    ///
    /// Everything else that used to depend on privileges now injects its
    /// failure in a way root cannot bypass — a rename onto a directory, a
    /// recreate path that escapes the root, `/proc` as a genuinely separate
    /// filesystem.
    fn require_unprivileged(property: &str) -> bool {
        if unsafe { libc::geteuid() } != 0 {
            return true;
        }
        if std::env::var_os(ROOT_SKIP_OPT_IN).is_some() {
            eprintln!(
                "[test] skipping (euid 0, {ROOT_SKIP_OPT_IN} set): {property} is not covered by \
                 this run"
            );
            return false;
        }
        panic!(
            "this test runs as root (euid 0), where file modes cannot deny access, so it would \
             prove nothing about: {property}. Run the suite as an ordinary user, or set \
             {ROOT_SKIP_OPT_IN}=1 to declare that this coverage is knowingly given up."
        );
    }

    /// Full recursive description of a tree: relative path, kind, mode, and
    /// file contents or symlink target. Two equal snapshots mean nothing
    /// changed, down to the permission bits and the bytes.
    fn snapshot(root: &Path) -> Vec<String> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
            let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)
                .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
                .map(|e| e.unwrap())
                .collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let path = entry.path();
                let rel = path.strip_prefix(base).unwrap().display().to_string();
                let meta = fs::symlink_metadata(&path).unwrap();
                let mode = meta.permissions().mode() & 0o7777;
                if meta.file_type().is_symlink() {
                    out.push(format!(
                        "{rel} symlink -> {}",
                        fs::read_link(&path).unwrap().display()
                    ));
                } else if meta.is_dir() {
                    out.push(format!("{rel} dir {mode:04o}"));
                    walk(&path, base, out);
                } else {
                    out.push(format!(
                        "{rel} file {mode:04o} {:?}",
                        fs::read(&path).unwrap()
                    ));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out
    }

    /// [`snapshot`], minus every top-level entry whose name starts with
    /// `prefix`. For the tests that deliberately break a write and do not care
    /// about the debris (a leftover `<name>.tmp`) it leaves behind.
    fn snapshot_excluding(root: &Path, prefix: &str) -> Vec<String> {
        snapshot(root)
            .into_iter()
            .filter(|line| !line.starts_with(prefix))
            .collect()
    }

    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// A root shaped roughly like a real `/data`: QBEE state with a private
    /// mode on it, ext4's `lost+found`, the agent's own tree, and assorted
    /// junk that must not survive.
    fn populate_realistic_root(root: &Path) {
        write_file(&root.join("qbee").join("device.json"), b"enrolment");
        write_file(&root.join("qbee").join("keys").join("id_rsa"), b"secret");
        fs::set_permissions(
            root.join("qbee").join("keys"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();

        fs::create_dir_all(root.join("lost+found")).unwrap();

        write_file(&root.join("fiber").join("fiber_medical.db"), b"patients");
        write_file(&root.join("fiber").join("standby.json"), b"{}");
        write_file(&root.join("chirpstack").join("chirpstack.db"), b"gateways");
        write_file(&root.join("lorawan").join("cluster.json"), b"{}");
        write_file(&root.join("loose-file.log"), b"log line");
        fs::create_dir_all(root.join("empty-dir")).unwrap();
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    // --- the plan itself --------------------------------------------------

    /// The production paths and modes are a contract with meta-fiber (the unit
    /// file and the LoRaWAN recipes) — asserted, never executed.
    #[test]
    fn production_plan_matches_what_meta_fiber_installs() {
        let plan = ResetPlan::production();
        assert_eq!(plan.root, PathBuf::from("/data"));
        assert_eq!(
            plan.preserve,
            vec![
                OsString::from("qbee"),
                OsString::from("lost+found"),
                OsString::from("factory_reset_in_progress.json"),
            ]
        );
        assert_eq!(
            plan.recreate,
            vec![
                (PathBuf::from("chirpstack"), 0o700),
                (PathBuf::from("lorawan"), 0o755),
            ]
        );
        assert_eq!(
            plan.mount_guard,
            MountGuard::DistinctDeviceFrom(PathBuf::from("/")),
            "production must never opt out of the mount check"
        );
        assert_eq!(
            plan.marker_path(),
            PathBuf::from("/data/factory_reset_in_progress.json")
        );
        // Inside /data/fiber, NOT /data: `fiber_app` reads this file and has to
        // delete it after consuming it, and its sandbox
        // (ProtectSystem=strict + ReadWritePaths=/data/fiber) makes /data itself
        // read-only. A result file it cannot unlink is re-consumed on every
        // boot — a permanent standby loop for post_action=power_off. See
        // `ResetPlan::result_path`.
        assert_eq!(
            plan.result_path(),
            PathBuf::from("/data/fiber/factory_reset_result.json")
        );
        assert!(
            plan.result_path().starts_with("/data/fiber/"),
            "the result file must live in the only directory fiber_app's sandbox can write to, \
             or it can never be cleared"
        );
        // The marker, by contrast, must stay directly under the wipe root: it
        // has to survive the wipe, and only the unsandboxed executor touches it.
        assert!(!plan.marker_path().starts_with("/data/fiber/"));
    }

    /// The ledger lives inside the wipe area and is therefore destroyed by the
    /// wipe itself — which is why `WipeAttempt::recovered_request` exists.
    #[test]
    fn the_ledger_is_not_in_the_preserve_set() {
        let plan = ResetPlan::production();
        assert!(
            LEDGER_PATH.starts_with("/data/fiber/"),
            "the ledger is expected to live inside the wipe area"
        );
        assert!(!plan.is_preserved(OsStr::new("fiber")));
    }

    // --- guardrails -------------------------------------------------------

    #[test]
    fn qbee_as_a_symlink_is_refused_and_nothing_is_touched() {
        let outside = tempfile::tempdir().unwrap();
        let real_qbee = outside.path().join("real-qbee");
        write_file(&real_qbee.join("device.json"), b"enrolment");

        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        fs::remove_dir_all(dir.path().join("qbee")).unwrap();
        std::os::unix::fs::symlink(&real_qbee, dir.path().join("qbee")).unwrap();

        let before = snapshot(dir.path());
        let plan = test_plan(dir.path());
        let outcome = execute(&plan, &sample());

        assert_eq!(outcome.status, ResetStatus::Failed);
        assert!(
            outcome.errors.iter().any(|e| e.contains("symlink")),
            "the error must name the symlink: {:?}",
            outcome.errors
        );
        assert_eq!(
            snapshot(dir.path()),
            before,
            "a refused wipe must leave the tree byte-identical — no marker, no result file"
        );
        assert!(
            real_qbee.join("device.json").exists(),
            "the symlink's target must not have been followed"
        );
        assert!(!plan.marker_path().exists());
        assert!(!plan.result_path().exists());
    }

    #[test]
    fn a_symlinked_wipe_root_is_refused() {
        let real = tempfile::tempdir().unwrap();
        populate_realistic_root(real.path());
        let before = snapshot(real.path());

        let holder = tempfile::tempdir().unwrap();
        let link = holder.path().join("data");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();

        let outcome = execute(&test_plan(&link), &sample());

        assert_eq!(outcome.status, ResetStatus::Failed);
        assert_eq!(snapshot(real.path()), before);
    }

    #[test]
    fn a_missing_wipe_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = execute(&test_plan(&dir.path().join("not-mounted-here")), &sample());
        assert_eq!(outcome.status, ResetStatus::Failed);
    }

    #[test]
    fn a_wipe_root_that_is_a_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("regular-file");
        write_file(&file, b"not a directory");
        let outcome = execute(&test_plan(&file), &sample());
        assert_eq!(outcome.status, ResetStatus::Failed);
        assert_eq!(fs::read(&file).unwrap(), b"not a directory");
    }

    /// The `/data`-is-not-mounted case, with the real `st_dev` comparison: the
    /// reference path is the tempdir itself, so it is trivially the same
    /// filesystem as the root.
    #[test]
    fn mount_guard_refuses_a_root_on_the_same_filesystem_as_its_reference() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        let before = snapshot(dir.path());

        let mut plan = test_plan(dir.path());
        plan.mount_guard = MountGuard::DistinctDeviceFrom(dir.path().to_path_buf());

        let outcome = execute(&plan, &sample());

        assert_eq!(outcome.status, ResetStatus::Failed);
        assert!(
            outcome.errors.iter().any(|e| e.contains("not mounted")),
            "the error must say the partition is not mounted: {:?}",
            outcome.errors
        );
        assert_eq!(snapshot(dir.path()), before, "nothing may be touched");
        assert!(!plan.marker_path().exists());
        assert!(!plan.result_path().exists());
    }

    /// The production guard makes the worst possible mistake structurally
    /// impossible: a plan rooted at `/` can never pass a check that requires
    /// `root` to be on a different filesystem from `/`. Read-only — this
    /// calls the guardrail, never [`execute`].
    #[test]
    fn a_plan_rooted_at_slash_cannot_pass_the_production_guard() {
        let plan = ResetPlan::production();
        let rootfs_plan = ResetPlan {
            root: PathBuf::from("/"),
            ..plan
        };
        let err = check_guardrails(&rootfs_plan)
            .expect_err("wiping / must be structurally impossible, not merely unlikely");
        assert!(err.contains("same filesystem"), "{err}");
    }

    /// The accept direction of the same check, with two genuinely different
    /// filesystems and no dependence on how the host laid out `/tmp`.
    ///
    /// `/proc` is the reference: procfs is always mounted on Linux (the only
    /// platform this crate builds for) and always has an `st_dev` of its own,
    /// so a tempdir is guaranteed to be "on a different filesystem from the
    /// reference" — the exact comparison production makes against `/`, run
    /// against real `stat` results. Earlier this test compared against `/` and
    /// skipped itself whenever the tempdir happened to share a filesystem with
    /// it, which is the common case inside a CI container: the accept direction
    /// of the mount guard then went untested precisely where it mattered.
    #[test]
    fn mount_guard_accepts_a_root_on_a_genuinely_separate_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let proc_dev = fs::metadata("/proc").unwrap().dev();
        let root_dev = fs::metadata(dir.path()).unwrap().dev();
        assert_ne!(
            proc_dev, root_dev,
            "procfs must be a separate filesystem for this test to mean anything"
        );

        let mut plan = test_plan(dir.path());
        plan.mount_guard = MountGuard::DistinctDeviceFrom(PathBuf::from("/proc"));
        assert!(
            check_guardrails(&plan).is_ok(),
            "a root on its own filesystem must pass"
        );

        // And the same plan, with the root swapped for a directory that *is*
        // on the reference filesystem, must be refused for that reason — so
        // this is not passing for some unrelated one.
        let mut same_fs = plan.clone();
        same_fs.root = PathBuf::from("/proc");
        let err = check_guardrails(&same_fs).expect_err("same filesystem must be refused");
        assert!(err.contains("same filesystem"), "{err}");
    }

    // --- the wipe pass ----------------------------------------------------

    #[test]
    fn wipe_root_keeps_exactly_the_preserve_set() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        // The marker a real run would have written before this point.
        write_file(&dir.path().join(IN_PROGRESS_MARKER_NAME), b"{}");

        let tally = wipe_root(&test_plan(dir.path()));

        assert!(tally.errors.is_empty(), "{:?}", tally.errors);
        assert!(!tally.unreadable_root);
        assert_eq!(tally.preserved, 3, "qbee, lost+found and the marker");

        let mut survivors: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        survivors.sort();
        assert_eq!(
            survivors,
            vec![IN_PROGRESS_MARKER_NAME, "lost+found", "qbee"],
            "the in-progress marker must survive the pass that would otherwise delete its own \
             crash-recovery record"
        );
    }

    #[test]
    fn qbee_survives_bit_for_bit_including_permissions() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        let qbee_before = snapshot(&dir.path().join("qbee"));

        let outcome = execute(&test_plan(dir.path()), &sample());

        assert_eq!(
            outcome.status,
            ResetStatus::Completed,
            "{:?}",
            outcome.errors
        );
        assert_eq!(
            snapshot(&dir.path().join("qbee")),
            qbee_before,
            "QBEE state is how a wiped gateway is reachable at all — it must be untouched"
        );
        assert_eq!(mode_of(&dir.path().join("qbee").join("keys")), 0o700);
        assert!(dir.path().join("lost+found").is_dir());
        // `fiber/` itself comes back, because the result file is written into it
        // (see `ResetPlan::result_path`) — but nothing that was in it survives.
        assert!(
            !dir.path().join("fiber").join("fiber_medical.db").exists(),
            "the agent's own data, including the ledger, is wiped"
        );
        assert!(!dir.path().join("fiber").join("standby.json").exists());
        assert_eq!(
            fs::read_dir(dir.path().join("fiber")).unwrap().count(),
            1,
            "the only thing left under fiber/ is the result file this run wrote"
        );
        assert!(!dir.path().join("loose-file.log").exists());
        assert!(!dir.path().join("empty-dir").exists());
    }

    #[test]
    fn recreated_directories_get_the_exact_modes_lorawan_needs() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());

        let outcome = execute(&test_plan(dir.path()), &sample());

        assert_eq!(outcome.status, ResetStatus::Completed);
        assert!(dir.path().join("chirpstack").is_dir());
        assert!(dir.path().join("lorawan").is_dir());
        assert_eq!(mode_of(&dir.path().join("chirpstack")), 0o700);
        assert_eq!(mode_of(&dir.path().join("lorawan")), 0o755);
        // Recreated empty, not preserved: the old contents were wiped.
        assert_eq!(
            fs::read_dir(dir.path().join("chirpstack")).unwrap().count(),
            0
        );
        assert_eq!(outcome.recreated.len(), 2);
    }

    #[test]
    fn a_symlink_in_the_wipe_area_is_unlinked_without_following_it() {
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("precious.txt");
        write_file(&target, b"must survive");
        let target_dir = outside.path().join("precious-dir");
        write_file(&target_dir.join("inner.txt"), b"must also survive");

        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        // A symlink to a file, a symlink to a directory, and one of each
        // nested inside a directory that gets recursively removed — the case
        // where a `remove_dir_all` that followed symlinks would do the damage.
        std::os::unix::fs::symlink(&target, dir.path().join("link-to-file")).unwrap();
        std::os::unix::fs::symlink(&target_dir, dir.path().join("link-to-dir")).unwrap();
        fs::create_dir_all(dir.path().join("nested")).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("nested").join("deep-file-link"))
            .unwrap();
        std::os::unix::fs::symlink(&target_dir, dir.path().join("nested").join("deep-dir-link"))
            .unwrap();

        let outcome = execute(&test_plan(dir.path()), &sample());

        assert_eq!(
            outcome.status,
            ResetStatus::Completed,
            "{:?}",
            outcome.errors
        );
        assert!(!dir.path().join("link-to-file").exists());
        assert!(!dir.path().join("nested").exists());
        assert_eq!(
            fs::read(&target).unwrap(),
            b"must survive",
            "the wipe must unlink symlinks, never follow them out of the root"
        );
        assert_eq!(
            fs::read(target_dir.join("inner.txt")).unwrap(),
            b"must also survive",
            "std's remove_dir_all must not recurse through a symlinked subdirectory"
        );
        assert!(
            outside.path().join("precious-dir").is_dir(),
            "the symlinked directory itself must still be there"
        );
    }

    /// The real-filesystem version of the partial-failure contract: an actual
    /// `remove_dir_all` denied by actual mode bits.
    ///
    /// Root bypasses mode bits, so this is one of the two tests gated on being
    /// an ordinary user — loudly, via [`require_unprivileged`], not with a
    /// silent early return. `a_recreate_error_alone_degrades_the_status_and_still_
    /// creates_the_rest` covers the same status/result/recreate contract with a
    /// privilege-independent injection, so a root run is not left with zero
    /// coverage of it.
    #[test]
    fn an_entry_that_cannot_be_removed_still_leaves_the_recreated_dirs_behind() {
        if !require_unprivileged(
            "a removal denied by filesystem permissions is collected as a \
                                  per-entry error, the wipe loop carries on past it, and the \
                                  recreate step still runs",
        ) {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        // A directory whose own mode forbids unlinking its children, so
        // `remove_dir_all` fails part-way — the "one stuck file" case.
        let stubborn = dir.path().join("stubborn");
        write_file(&stubborn.join("locked.bin"), b"cannot go");
        fs::set_permissions(&stubborn, fs::Permissions::from_mode(0o500)).unwrap();

        let plan = test_plan(dir.path());
        let outcome = execute(&plan, &sample());

        assert_eq!(
            outcome.status,
            ResetStatus::CompletedWithErrors,
            "a single stuck entry is not a failed wipe"
        );
        assert!(
            outcome.errors.iter().any(|e| e.contains("stubborn")),
            "the stuck path must be named in the result: {:?}",
            outcome.errors
        );
        assert!(stubborn.exists(), "the stuck entry is still there");
        assert!(
            !dir.path().join("loose-file.log").exists(),
            "the loop must have carried on past the stuck entry"
        );
        assert!(!dir.path().join("fiber").join("fiber_medical.db").exists());
        // The point of the test: partial failure must not skip the recreate.
        assert_eq!(mode_of(&dir.path().join("chirpstack")), 0o700);
        assert_eq!(mode_of(&dir.path().join("lorawan")), 0o755);
        assert_eq!(outcome.recreated.len(), 2);

        let result = ResetOutcome::read(&plan.result_path())
            .expect("a result file must be written even on partial failure");
        assert_eq!(result.status, ResetStatus::CompletedWithErrors);
        assert_eq!(result, outcome);

        // Let the tempdir clean itself up.
        fs::set_permissions(&stubborn, fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// The same contract as the permission-denied test above — errors are
    /// collected, the status degrades, the valid recreate entries still land,
    /// and a result file is written — but injected through a path the engine
    /// itself refuses rather than through mode bits. Runs everywhere, root or
    /// not, which is what keeps CI's signal from being weaker than a local run.
    #[test]
    fn a_recreate_error_alone_degrades_the_status_and_still_creates_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        let mut plan = test_plan(dir.path());
        plan.recreate.push((PathBuf::from("../escaped"), 0o755));

        let outcome = execute(&plan, &sample());

        assert_eq!(outcome.status, ResetStatus::CompletedWithErrors);
        assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
        assert!(outcome.errors[0].contains("escapes"));
        // The wipe itself still happened, and the legitimate directories are
        // still back with the right modes.
        assert!(!dir.path().join("fiber").join("fiber_medical.db").exists());
        assert_eq!(mode_of(&dir.path().join("chirpstack")), 0o700);
        assert_eq!(mode_of(&dir.path().join("lorawan")), 0o755);
        assert!(!dir.path().parent().unwrap().join("escaped").exists());
        assert_eq!(
            ResetOutcome::read(&plan.result_path()).unwrap(),
            outcome,
            "a partial failure must still leave a durable result"
        );
        assert!(!plan.marker_path().exists());
    }

    /// `set_permissions` follows symlinks and `create_dir_all` accepts a
    /// symlink that already resolves to a directory, so a link surviving under
    /// a recreate name would otherwise get the target chmod'ed.
    #[test]
    fn recreate_refuses_to_chmod_through_a_symlink() {
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("someone-elses-dir");
        fs::create_dir_all(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut plan = test_plan(dir.path());
        plan.recreate = vec![(PathBuf::from("chirpstack"), 0o700)];
        std::os::unix::fs::symlink(&target, dir.path().join("chirpstack")).unwrap();

        let (created, errors) = recreate_dirs(&plan);

        assert!(created.is_empty());
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("not a real directory"), "{errors:?}");
        assert_eq!(
            mode_of(&target),
            0o755,
            "the symlink's target must not have been chmod'ed through"
        );
    }

    #[test]
    fn recreate_refuses_a_path_that_would_escape_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let mut plan = test_plan(dir.path());
        plan.recreate = vec![
            (PathBuf::from("/etc/absolute"), 0o755),
            (PathBuf::from("../escaped"), 0o755),
            (PathBuf::from("fine"), 0o750),
        ];

        let (created, errors) = recreate_dirs(&plan);

        assert_eq!(created.len(), 1);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(dir.path().join("fine").is_dir());
        assert_eq!(mode_of(&dir.path().join("fine")), 0o750);
        assert!(
            !dir.path().parent().unwrap().join("escaped").exists(),
            "nothing may be created outside the wipe root"
        );
    }

    // --- marker, result, resumability -------------------------------------

    #[test]
    fn execute_writes_a_result_and_clears_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        let plan = test_plan(dir.path());

        let outcome = execute(&plan, &sample());

        assert!(
            !plan.marker_path().exists(),
            "the marker is cleared once the result is durable"
        );
        let result = ResetOutcome::read(&plan.result_path()).expect("result must be readable");
        assert_eq!(result, outcome);
        assert_eq!(result.attempts, 1);
        assert_eq!(result.status, ResetStatus::Completed);
        assert!(result.preserved >= 2);
        assert!(result.removed >= 4);
    }

    /// Regression: the result file must land inside `<root>/fiber`, the only
    /// directory `fiber_app`'s systemd sandbox (`ProtectSystem=strict` +
    /// `ReadWritePaths=/data/fiber`) can write to.
    ///
    /// It used to be written directly under `<root>` — i.e. `/data` — where
    /// `fiber_app`'s boot hook could read it but *never unlink* it (`EROFS`,
    /// swallowed by the best-effort `ResetOutcome::clear`). Every subsequent
    /// boot then re-consumed the same result: an endless standby re-entry for
    /// `PostResetAction::PowerOff` (a device recoverable only over SSH/QBEE),
    /// and a duplicate `FACTORY_RESET_COMPLETED` audit row per boot otherwise.
    /// Phase 2 is unsandboxed, so writing one directory deeper costs it nothing.
    #[test]
    fn the_result_file_lands_where_the_agents_sandbox_can_delete_it() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        let plan = test_plan(dir.path());

        execute(&plan, &sample());

        assert_eq!(
            plan.result_path(),
            dir.path().join("fiber").join(RESULT_FILE_NAME)
        );
        assert!(plan.result_path().is_file(), "the result must be there");
        assert!(
            !dir.path().join(RESULT_FILE_NAME).exists(),
            "nothing may be left at the old, un-deletable location directly under the wipe root"
        );
        // And the whole point: it really is removable from that location.
        ResetOutcome::clear(&plan.result_path());
        assert!(!plan.result_path().exists());
    }

    /// The result file carries forward everything the wipe just destroyed the
    /// original copy of.
    #[test]
    fn the_result_carries_the_ledger_fields_forward() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        let request = ResetRequest::new(
            PostResetAction::PowerOff,
            "decommissioning ward 3".to_string(),
            "dr.jane".to_string(),
            "challenge-request-id-42".to_string(),
        );

        let plan = test_plan(dir.path());
        execute(&plan, &request);

        let result = ResetOutcome::read(&plan.result_path()).unwrap();
        assert_eq!(result.reason, "decommissioning ward 3");
        assert_eq!(result.requested_by, "dr.jane");
        assert_eq!(result.request_id, "challenge-request-id-42");
        assert_eq!(
            result.post_action,
            PostResetAction::PowerOff,
            "phase 2 does not apply post_action itself — it hands it on"
        );
    }

    #[test]
    fn a_second_run_over_an_already_wiped_root_is_harmless() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        let plan = test_plan(dir.path());

        let first = execute(&plan, &sample());
        assert_eq!(first.status, ResetStatus::Completed);

        let second = execute(&plan, &sample());
        assert_eq!(second.status, ResetStatus::Completed);
        assert_eq!(
            second.attempts, 1,
            "the first run cleared its marker, so this is a fresh attempt"
        );
        assert_eq!(mode_of(&dir.path().join("chirpstack")), 0o700);
        assert!(dir.path().join("qbee").join("device.json").exists());
    }

    #[test]
    fn an_existing_marker_continues_the_attempt_count() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        let plan = test_plan(dir.path());

        WipeAttempt {
            schema_version: CURRENT_SCHEMA_VERSION,
            request_id: "req-1".to_string(),
            started_at_unix: 1_000,
            attempts: 1,
        }
        .write(&plan.marker_path())
        .unwrap();

        let outcome = execute(&plan, &sample());

        assert_eq!(
            outcome.attempts, 2,
            "a power cut mid-wipe resumes, not restarts"
        );
        assert_eq!(outcome.status, ResetStatus::Completed);
    }

    #[test]
    fn the_attempt_cap_stops_a_wipe_that_keeps_killing_the_device() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        let plan = test_plan(dir.path());

        WipeAttempt {
            schema_version: CURRENT_SCHEMA_VERSION,
            request_id: "req-1".to_string(),
            started_at_unix: 1_000,
            attempts: MAX_WIPE_ATTEMPTS,
        }
        .write(&plan.marker_path())
        .unwrap();

        let outcome = execute(&plan, &sample());

        assert_eq!(outcome.status, ResetStatus::Failed);
        assert_eq!(outcome.attempts, MAX_WIPE_ATTEMPTS);
        assert!(
            outcome.errors.iter().any(|e| e.contains("abandoned")),
            "{:?}",
            outcome.errors
        );
        assert!(
            dir.path().join("fiber").join("fiber_medical.db").exists(),
            "giving up means not wiping — the data is left alone"
        );
        assert_eq!(
            ResetOutcome::read(&plan.result_path()).unwrap().status,
            ResetStatus::Failed
        );
        // Still done, because LoRaWAN needs it regardless.
        assert_eq!(mode_of(&dir.path().join("chirpstack")), 0o700);
        assert_eq!(mode_of(&dir.path().join("lorawan")), 0o755);

        // The marker must NOT be cleared: clearing it would zero the attempt
        // count and let the very next boot start the whole wipe again, which is
        // the loop the cap exists to prevent. So the refusal has to be stable
        // across boots — assert that by running it again.
        assert_eq!(
            WipeAttempt::read(&plan.marker_path()).unwrap().attempts,
            MAX_WIPE_ATTEMPTS
        );
        let again = execute(&plan, &sample());
        assert_eq!(again.status, ResetStatus::Failed);
        assert!(
            dir.path().join("fiber").join("fiber_medical.db").exists(),
            "still not wiped"
        );
    }

    /// The other half of keeping that marker: a newly signed reset must not
    /// inherit an abandoned one's exhausted attempt count.
    #[test]
    fn a_new_request_id_is_not_blocked_by_an_abandoned_wipes_marker() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        let plan = test_plan(dir.path());

        WipeAttempt {
            schema_version: CURRENT_SCHEMA_VERSION,
            request_id: "old-abandoned-request".to_string(),
            started_at_unix: 1_000,
            attempts: MAX_WIPE_ATTEMPTS,
        }
        .write(&plan.marker_path())
        .unwrap();

        let mut fresh = sample();
        fresh.request_id = "newly-signed-request".to_string();
        let outcome = execute(&plan, &fresh);

        assert_eq!(
            outcome.attempts, 1,
            "a different request id starts its own attempt count"
        );
        assert_eq!(outcome.status, ResetStatus::Completed);
        assert!(!dir.path().join("fiber").join("fiber_medical.db").exists());
    }

    /// Failure injected by putting a *directory* where the marker file has to
    /// go: the atomic write's final `rename` of a regular file onto a directory
    /// fails with `EISDIR` for everyone, root included. No permission bits
    /// involved, so this covers the property under CI's root environment too.
    #[test]
    fn a_wipe_whose_attempt_cannot_be_recorded_does_not_start() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        let plan = test_plan(dir.path());
        fs::create_dir_all(plan.marker_path()).unwrap();

        let before = snapshot_excluding(dir.path(), IN_PROGRESS_MARKER_NAME);

        let outcome = execute(&plan, &sample());

        assert_eq!(outcome.status, ResetStatus::Failed);
        assert_eq!(outcome.attempts, 0);
        assert!(
            outcome.errors.iter().any(|e| e.contains("rename")),
            "the marker write must be what failed: {:?}",
            outcome.errors
        );
        assert_eq!(
            snapshot_excluding(dir.path(), IN_PROGRESS_MARKER_NAME),
            before,
            "an unaccounted wipe is worse than a refused one — nothing may be removed, and \
             nothing recreated either"
        );
        assert!(
            !plan.result_path().exists(),
            "no result file: this run never got as far as having a result"
        );
    }

    #[test]
    fn the_marker_round_trips_and_rejects_a_foreign_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(IN_PROGRESS_MARKER_NAME);
        let marker = WipeAttempt {
            schema_version: CURRENT_SCHEMA_VERSION,
            request_id: "req-1".to_string(),
            started_at_unix: 42,
            attempts: 2,
        };
        marker.write(&path).unwrap();
        assert_eq!(WipeAttempt::read(&path).unwrap(), marker);

        WipeAttempt {
            schema_version: CURRENT_SCHEMA_VERSION + 1,
            ..marker.clone()
        }
        .write(&path)
        .unwrap();
        assert!(WipeAttempt::read(&path).is_none());

        fs::write(&path, b"{ truncated").unwrap();
        assert!(WipeAttempt::read(&path).is_none());

        WipeAttempt::clear(&path);
        assert!(WipeAttempt::read(&path).is_none());
        // Idempotent.
        WipeAttempt::clear(&path);
    }

    #[test]
    fn the_result_file_rejects_a_foreign_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RESULT_FILE_NAME);
        let mut outcome = ResetOutcome::new(&sample(), ResetStatus::Completed, 1);
        write_json_durably(&path, &outcome).unwrap();
        assert!(ResetOutcome::read(&path).is_some());

        outcome.schema_version = CURRENT_SCHEMA_VERSION + 1;
        write_json_durably(&path, &outcome).unwrap();
        assert!(ResetOutcome::read(&path).is_none());
    }

    // --- un-arming the request -------------------------------------------

    /// The fix for the review's Important #1. Clearing the ledger on a `Failed`
    /// outcome meant a refused wipe erased the only durable record that an
    /// operator had signed a destructive command, and guaranteed it would never
    /// be retried.
    #[test]
    fn only_a_wipe_that_happened_un_arms_the_ledger() {
        assert!(should_clear_ledger(ResetStatus::Completed));
        assert!(
            should_clear_ledger(ResetStatus::CompletedWithErrors),
            "the wipe ran; the result file is what reports the leftovers"
        );
        assert!(
            !should_clear_ledger(ResetStatus::Failed),
            "a refusal must leave the request armed, visible and retryable"
        );
    }

    /// End to end over a refusal: the ledger written by phase 1 is still there
    /// afterwards, and a later boot that no longer trips the guardrail wipes.
    #[test]
    fn a_refused_wipe_leaves_a_ledger_the_next_boot_can_still_act_on() {
        let outside = tempfile::tempdir().unwrap();
        let real_qbee = outside.path().join("real-qbee");
        fs::create_dir_all(&real_qbee).unwrap();

        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        let plan = test_plan(dir.path());

        // Phase 1's ledger lives inside the wipe area.
        let ledger_path = dir.path().join("fiber").join("factory_reset_request.json");
        let request = sample();
        request.write(&ledger_path).unwrap();

        // Boot 1: qbee is a symlink, so the wipe is refused.
        fs::remove_dir_all(dir.path().join("qbee")).unwrap();
        std::os::unix::fs::symlink(&real_qbee, dir.path().join("qbee")).unwrap();
        let refused = execute(&plan, &request);
        assert_eq!(refused.status, ResetStatus::Failed);
        assert!(!should_clear_ledger(refused.status));
        assert_eq!(
            ResetRequest::read(&ledger_path).as_ref(),
            Some(&request),
            "the armed request must survive a refusal — it is the only durable evidence"
        );

        // Boot 2: whatever put that symlink there is gone. The still-armed
        // ledger is what makes the retry possible.
        fs::remove_file(dir.path().join("qbee")).unwrap();
        fs::create_dir_all(dir.path().join("qbee")).unwrap();
        let retried = execute(&plan, &ResetRequest::read(&ledger_path).unwrap());
        assert_eq!(retried.status, ResetStatus::Completed);
        assert_eq!(
            retried.request_id, request.request_id,
            "and it is still the same signed request being honoured"
        );
        assert!(!ledger_path.exists(), "the wipe took the ledger with it");
    }

    // --- boot decision ----------------------------------------------------

    fn marker_for(request_id: &str) -> WipeAttempt {
        WipeAttempt {
            schema_version: CURRENT_SCHEMA_VERSION,
            request_id: request_id.to_string(),
            started_at_unix: 1_000,
            attempts: 1,
        }
    }

    #[test]
    fn an_ordinary_boot_does_nothing() {
        assert_eq!(decide_boot_action(None, None, 10_000), BootAction::Nothing);
    }

    #[test]
    fn a_fresh_ledger_wipes() {
        let mut request = sample();
        request.requested_at_unix = 10_000;
        assert_eq!(
            decide_boot_action(Some(request.clone()), None, 10_030),
            BootAction::Wipe(request)
        );
    }

    #[test]
    fn a_ledger_older_than_a_day_is_discarded_not_acted_on() {
        let mut request = sample();
        request.requested_at_unix = 1_000;
        let now = 1_000 + STALE_LEDGER_AGE_SECS + 1;
        assert_eq!(
            decide_boot_action(Some(request.clone()), None, now),
            BootAction::StaleRequest(request.clone())
        );

        // Exactly at the limit is still actionable.
        assert_eq!(
            decide_boot_action(Some(request.clone()), None, 1_000 + STALE_LEDGER_AGE_SECS),
            BootAction::Wipe(request)
        );
    }

    /// The RTC is synced by a separate oneshot unit, so at early boot the
    /// clock can read *earlier* than the ledger was written. That must not
    /// underflow into "ancient" and silently discard a real request.
    #[test]
    fn a_clock_that_reads_before_the_request_does_not_make_it_stale() {
        let mut request = sample();
        request.requested_at_unix = 1_700_000_000;
        assert_eq!(
            decide_boot_action(Some(request.clone()), None, 0),
            BootAction::Wipe(request)
        );
    }

    /// A half-finished wipe still has to be finished, however long the device
    /// sat in a drawer first.
    #[test]
    fn an_interrupted_wipe_is_resumed_regardless_of_age() {
        let mut request = sample();
        request.requested_at_unix = 1_000;
        let ancient = 1_000 + STALE_LEDGER_AGE_SECS * 365;
        assert_eq!(
            decide_boot_action(Some(request.clone()), Some(marker_for("req-1")), ancient),
            BootAction::Wipe(request)
        );
    }

    /// The wipe destroys `/data/fiber` and the ledger with it, so a power cut
    /// just after that leaves a marker and no ledger — and the recreate step
    /// has not run yet, which is why this must not be a no-op.
    #[test]
    fn a_marker_without_a_ledger_resumes_from_the_marker() {
        let action = decide_boot_action(None, Some(marker_for("challenge-42")), 10_000);
        match action {
            BootAction::Wipe(request) => {
                assert_eq!(
                    request.request_id, "challenge-42",
                    "the id the server side already knows must survive"
                );
                assert_eq!(request.post_action, PostResetAction::Reboot);
                assert!(request.reason.contains("unknown"));
            }
            other => panic!("expected a resumed wipe, got {other:?}"),
        }
    }

    // --- main.rs's boot hook: post-reset action ----------------------------

    fn outcome_with(status: ResetStatus, post_action: PostResetAction) -> ResetOutcome {
        let mut request = sample();
        request.post_action = post_action;
        let mut outcome = ResetOutcome::new(&request, status, 1);
        outcome.removed = 7;
        outcome.preserved = 2;
        outcome
    }

    #[test]
    fn no_result_file_means_nothing_to_do() {
        assert_eq!(
            decide_post_reset_boot_action(None),
            PostResetBootAction::None
        );
    }

    #[test]
    fn a_completed_reboot_result_is_consumed_without_entering_standby() {
        let outcome = outcome_with(ResetStatus::Completed, PostResetAction::Reboot);
        assert_eq!(
            decide_post_reset_boot_action(Some(&outcome)),
            PostResetBootAction::Consume {
                enter_standby: false
            },
            "the boot itself is the requested reboot — no extra action"
        );
    }

    #[test]
    fn a_completed_power_off_result_enters_standby() {
        let outcome = outcome_with(ResetStatus::Completed, PostResetAction::PowerOff);
        assert_eq!(
            decide_post_reset_boot_action(Some(&outcome)),
            PostResetBootAction::Consume {
                enter_standby: true
            }
        );
    }

    /// `status` must not change the decision: a wipe that partially failed
    /// does not change what the operator asked for the device to do
    /// afterwards, and the failure is still recorded in the
    /// `FACTORY_RESET_COMPLETED` audit row regardless of whether standby is
    /// entered.
    #[test]
    fn a_completed_with_errors_power_off_result_still_enters_standby() {
        let outcome = outcome_with(ResetStatus::CompletedWithErrors, PostResetAction::PowerOff);
        assert_eq!(
            decide_post_reset_boot_action(Some(&outcome)),
            PostResetBootAction::Consume {
                enter_standby: true
            }
        );
    }

    /// Even a wipe that could not be started at all (guardrail refusal,
    /// attempt cap reached) still gets the operator's requested power state:
    /// standby is reversible with a power cycle, and the `Failed` status is
    /// preserved in the audit row, so this is not a device silently going
    /// dark on unrecorded, unrecoverable failure.
    #[test]
    fn a_failed_power_off_result_still_enters_standby() {
        let outcome = outcome_with(ResetStatus::Failed, PostResetAction::PowerOff);
        assert_eq!(
            decide_post_reset_boot_action(Some(&outcome)),
            PostResetBootAction::Consume {
                enter_standby: true
            }
        );
    }

    #[test]
    fn a_failed_reboot_result_is_consumed_without_entering_standby() {
        let outcome = outcome_with(ResetStatus::Failed, PostResetAction::Reboot);
        assert_eq!(
            decide_post_reset_boot_action(Some(&outcome)),
            PostResetBootAction::Consume {
                enter_standby: false
            }
        );
    }

    #[test]
    fn clear_removes_the_result_file_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RESULT_FILE_NAME);
        let outcome = outcome_with(ResetStatus::Completed, PostResetAction::Reboot);
        write_json_durably(&path, &outcome).unwrap();
        assert!(path.exists());

        ResetOutcome::clear(&path);
        assert!(!path.exists());

        // Clearing again (nothing there) must not panic or error.
        ResetOutcome::clear(&path);
    }

    #[test]
    fn is_corrupt_is_false_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RESULT_FILE_NAME);
        assert!(!ResetOutcome::is_corrupt(&path));
    }

    #[test]
    fn is_corrupt_is_false_for_a_valid_current_schema_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RESULT_FILE_NAME);
        let outcome = outcome_with(ResetStatus::Completed, PostResetAction::Reboot);
        write_json_durably(&path, &outcome).unwrap();
        assert!(!ResetOutcome::is_corrupt(&path));
    }

    #[test]
    fn is_corrupt_is_true_for_unparseable_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RESULT_FILE_NAME);
        fs::write(&path, b"not json at all {{{").unwrap();
        assert!(ResetOutcome::is_corrupt(&path));
    }

    #[test]
    fn is_corrupt_is_true_for_a_foreign_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RESULT_FILE_NAME);
        let mut outcome = outcome_with(ResetStatus::Completed, PostResetAction::Reboot);
        outcome.schema_version = CURRENT_SCHEMA_VERSION + 1;
        write_json_durably(&path, &outcome).unwrap();
        assert!(ResetOutcome::is_corrupt(&path));
    }

    /// The whole point of `is_corrupt`: a file that merely could not be
    /// *read* this once (a permission hiccup standing in for any transient
    /// `fs::read` error) must never be treated the same as one that is
    /// genuinely, permanently unparseable. Getting this backwards means a
    /// boot-time glitch permanently destroys the only on-disk record of a
    /// reset's outcome — see the regression this guards against in `main.rs`.
    #[test]
    fn is_corrupt_is_false_for_a_file_that_exists_but_cannot_be_read() {
        if !require_unprivileged(
            "a file whose mode denies read access simulates a transient fs::read failure, and \
             root can read through any mode",
        ) {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RESULT_FILE_NAME);
        let outcome = outcome_with(ResetStatus::Completed, PostResetAction::Reboot);
        write_json_durably(&path, &outcome).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        assert!(
            !ResetOutcome::is_corrupt(&path),
            "an unreadable-but-present file must not be treated as corrupt"
        );

        // Let the tempdir clean itself up.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    // --- dry run ----------------------------------------------------------

    #[test]
    fn dry_run_touches_nothing_at_all() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        write_file(&dir.path().join(IN_PROGRESS_MARKER_NAME), b"{}");
        let before = snapshot(dir.path());

        let report = dry_run(&test_plan(dir.path()));

        assert_eq!(
            snapshot(dir.path()),
            before,
            "a dry run must be byte-identical in and out — this is what makes it safe on a \
             live device"
        );
        assert!(!report.render().is_empty());
    }

    #[test]
    fn dry_run_classifies_every_entry() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        write_file(&dir.path().join(IN_PROGRESS_MARKER_NAME), b"{}");

        let plan = test_plan(dir.path());
        let report = dry_run(&plan);

        let names = |paths: &Vec<PathBuf>| -> Vec<String> {
            let mut n: Vec<String> = paths
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            n.sort();
            n
        };

        assert_eq!(
            names(&report.would_preserve),
            vec!["factory_reset_in_progress.json", "lost+found", "qbee"]
        );
        assert_eq!(
            names(&report.would_remove),
            vec![
                "chirpstack",
                "empty-dir",
                "fiber",
                "loose-file.log",
                "lorawan"
            ]
        );
        assert_eq!(
            report.would_recreate,
            vec![
                (dir.path().join("chirpstack"), 0o700),
                (dir.path().join("lorawan"), 0o755),
            ]
        );
        assert!(report.errors.is_empty());
        assert!(report.guardrails.is_ok());
    }

    /// A dry run on a device where a real run would refuse says so, rather
    /// than printing a plan that would never execute.
    #[test]
    fn dry_run_reports_a_guardrail_that_would_refuse() {
        let dir = tempfile::tempdir().unwrap();
        populate_realistic_root(dir.path());
        let mut plan = test_plan(dir.path());
        plan.mount_guard = MountGuard::DistinctDeviceFrom(dir.path().to_path_buf());

        let report = dry_run(&plan);

        assert!(report.guardrails.is_err());
        assert!(report.render().contains("WOULD REFUSE"));
    }

    // --- no external commands --------------------------------------------

    /// Phase 2 must run entirely on `std::fs`. A shelled-out helper on a
    /// device whose partition may not be mounted the way we expect would make
    /// every guardrail above bypassable, and its failure modes invisible.
    ///
    /// Enforced by reading this file's own source below the phase-2 marker
    /// (phase 1's `systemctl` preflight above it is legitimate and stays), plus
    /// the whole of the executor binary. The needle is assembled with `concat!`
    /// so this test's own source does not contain the very text it forbids.
    #[test]
    fn phase_2_never_spawns_an_external_program() {
        let needle = concat!("Command", "::new");

        let this_file = include_str!("factory_reset.rs");
        let (phase_1, phase_2) = this_file
            .split_once(PHASE2_SECTION_MARKER)
            .expect("the phase-2 section marker must still be in this file");
        assert!(
            phase_1.contains(needle),
            "sanity check: phase 1's systemctl preflight lives above the marker, so the split \
             is the right way round"
        );
        assert!(
            !phase_2.contains(needle),
            "the phase-2 wipe engine must run no external programs"
        );

        let executor = include_str!("../bin/fiber_factory_reset.rs");
        assert!(
            !executor.contains(needle),
            "the phase-2 executor binary must run no external programs"
        );

        assert!(
            ALLOWED_EXTERNAL_COMMANDS.is_empty(),
            "the allowlist is empty by design; adding to it needs this test edited too"
        );
    }
}
