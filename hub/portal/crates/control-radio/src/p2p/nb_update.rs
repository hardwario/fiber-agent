//! When the Northbridge is updated — `plan/control/radio/northbridge_update.md` §3.4–§3.6 (U4).
//!
//! [`super::nb_flash`] knows *how* to flash; this module decides *whether*, keeps the retry
//! budget and answers the manual `northbridge-update` command:
//!
//! - **Automatic.** Each start of the P2P task, before the central opens the modem, the task
//!   probes the Northbridge (`GET_INFO`) and compares the image it reports (`app_valid` tail:
//!   length + CRC-32) with the bundled `northbridge.json` (§3.2). A mismatch in either
//!   direction flashes the bundled image, so a RAUC rollback brings the older image back.
//! - **Retry budget.** At most [`MAX_ATTEMPTS`] per (target image CRC-32, Hub boot id). The
//!   attempt is recorded *before* flashing, so a crash or power cut mid-flash still counts.
//!   After that the state is `failed` (a Hub alarm in the Portal) and the task runs with
//!   whatever the Northbridge has, if it answers.
//! - **Manual.** `proximosctl control.radio northbridge-update` writes
//!   `northbridge/update-request.json` (a generation bump, like `radio-set`); the running
//!   task stops its central, runs [`ensure`] with the request and writes the result back into
//!   `update-state.json`, which the CLI polls.
//!
//! A Northbridge that answers `GET_INFO` with 180 B has no `app_valid` tail and no
//! `ENTER_BOOTLOADER` (before 0.4.0): it needs the one-time J-Link migration of §4. That is a
//! note in the status, never an attempt, so it cannot become an alarm loop.
//!
//! The state lives under the radio store (`/data/proximos/radio/northbridge/`), the one
//! `ReadWritePaths` row of `proximos-control.service` — not `/data/proximos/northbridge/` as
//! §3.4 first said (amended 2026-10-08).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::gateway::{format_rfc3339, now_unix};
use crate::p2p::modem_io::ModemInfo;
use crate::p2p::nb_flash::{
    self, load_files, AppImageId, FlashPort, Manifest, Options, ProbeInfo, Progress,
    SerialFlashPort, VerifyMode,
};
use crate::p2p::northbridge::{NorthbridgeStatus, UpdateSummary};
use crate::store::{Slot, Store};

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

macro_rules! diag {
    ($($arg:tt)*) => { eprintln!("[p2p {}] {}", now_ms(), format_args!($($arg)*)) };
}

/// Where the image installs the bundled Northbridge firmware (§3.2, U3).
pub const BUNDLE_DIR: &str = "/usr/share/proximos/northbridge";
/// Overrides [`BUNDLE_DIR`], for tests and the bench.
pub const BUNDLE_ENV: &str = "PROXIMOS_NB_BUNDLE";
/// `both` or `readback` overrides the default checksum verification (U5's CRC-model check).
pub const VERIFY_ENV: &str = "PROXIMOS_NB_VERIFY";
/// The Hub's boot, half of the retry budget's key.
pub const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";
/// Attempts per (image, boot) pair (§3.4).
pub const MAX_ATTEMPTS: u32 = 3;
/// How often the running task looks for a new request — the same patience as `radio-set`.
pub const REQUEST_POLL: Duration = Duration::from_secs(2);
/// How long `northbridge-update` waits for its result by default.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(180);

const IMAGE: &str = "northbridge.bin";
const MANIFEST: &str = "northbridge.json";

/// [`BUNDLE_DIR`], or [`BUNDLE_ENV`].
pub fn bundle_dir() -> PathBuf {
    match std::env::var(BUNDLE_ENV) {
        Ok(p) if !p.trim().is_empty() => PathBuf::from(p.trim()),
        _ => PathBuf::from(BUNDLE_DIR),
    }
}

/// The bundled image and its manifest, checked against each other; `None` when the image
/// carries no Northbridge firmware (no files, or an empty package: the update is off).
pub fn load_bundle(dir: &Path) -> Result<Option<(Vec<u8>, Manifest)>> {
    let (bin, json) = (dir.join(IMAGE), dir.join(MANIFEST));
    if !bin.exists() && !json.exists() {
        return Ok(None);
    }
    load_files(&bin, &json).map(Some)
}

fn fw_err(kind: Kind, msg: impl Into<String>) -> Error {
    Error::new(kind, Transport::P2p, Component::NorthbridgeFirmware, msg)
}

/// What the update is doing — §3.5's vocabulary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateState {
    #[default]
    Idle,
    /// An attempt is under way. Still `flashing` after a restart means the last attempt
    /// never finished (a crash or a power cut); it was counted.
    Flashing,
    /// The retry budget is spent: a Hub alarm.
    Failed,
}

impl UpdateState {
    pub const fn as_str(self) -> &'static str {
        match self {
            UpdateState::Idle => "idle",
            UpdateState::Flashing => "flashing",
            UpdateState::Failed => "failed",
        }
    }
}

/// The retry budget's key: the image being flashed and the Hub boot it is spent in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetKey {
    pub crc32: u32,
    pub boot_id: String,
}

/// The last successful update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastOk {
    pub version: String,
    pub at: String,
    pub elapsed_ms: u64,
}

/// A hotfix (`--file`) kept over the bundled image: the automatic compare leaves the
/// Northbridge on it until the bundle changes (a RAUC update), instead of flashing the bundled
/// image back at the next start of the task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pinned {
    pub crc32: u32,
    pub version: String,
    /// The bundled image's CRC-32 when the hotfix went in; `None` without a bundle.
    pub bundled_crc32: Option<u32>,
}

/// The answer to one manual request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestResult {
    pub generation: u64,
    pub ok: bool,
    pub message: String,
}

/// `northbridge/update-state.json`. Written by the P2P task only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StateFile {
    pub state: UpdateState,
    /// The bundled image, `x.y.z+sha8`.
    pub bundled: Option<String>,
    pub budget: Option<BudgetKey>,
    /// Attempts spent on [`StateFile::budget`].
    pub attempts: u32,
    pub last_error: Option<String>,
    pub last_attempt_at: Option<String>,
    pub last_ok: Option<LastOk>,
    /// Not a failure, but worth showing: an image that needs the §4 migration.
    pub note: Option<String>,
    pub pinned: Option<Pinned>,
    /// The last request answered; a request with a higher generation is pending.
    pub request_done_generation: u64,
    pub request_result: Option<RequestResult>,
}

impl StateFile {
    /// The state on disk, or a fresh one when there is none. A file that does not parse is
    /// logged and replaced: the budget starts over rather than the update stopping for good.
    pub fn load(store: &Store) -> Result<StateFile> {
        match store.read(Slot::NbUpdateState)? {
            None => Ok(StateFile::default()),
            Some(raw) => Ok(serde_json::from_str(&raw).unwrap_or_else(|e| {
                diag!(
                    "northbridge update: {} does not parse ({e}); starting over",
                    store.path(Slot::NbUpdateState).display()
                );
                StateFile::default()
            })),
        }
    }

    pub fn save(&self, store: &Store) -> Result<()> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| fw_err(Kind::StorageError, format!("update-state.json: {e}")))?;
        store.write(Slot::NbUpdateState, &(text + "\n"))
    }

    /// Attempts already spent on `key`.
    pub fn attempts_for(&self, key: &BudgetKey) -> u32 {
        if self.budget.as_ref() == Some(key) {
            self.attempts
        } else {
            0
        }
    }

    /// The status document's `update` object.
    pub fn summary(&self) -> UpdateSummary {
        UpdateSummary {
            state: self.state.as_str(),
            bundled: self.bundled.clone(),
            attempts: self.attempts,
            last_error: self.last_error.clone(),
            note: self.note.clone(),
        }
    }
}

/// The `update` object for a task that does not run [`ensure`] on start — the idle
/// Northbridge task on a Hub without P2P (#158): the state on disk with the bundled image
/// named. `None` when the image bundles no firmware and nothing is worth showing, as
/// [`ensure`] reports it.
pub fn idle_summary(store: &Store, bundle: &Path) -> Option<UpdateSummary> {
    let mut state = StateFile::load(store).ok()?;
    state.bundled = load_bundle(bundle)
        .ok()
        .flatten()
        .map(|(_, m)| m.version_string());
    (state.bundled.is_some() || state.note.is_some()).then(|| state.summary())
}

/// Where a manual request's image comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The image's own `/usr/share/proximos/northbridge/`.
    Bundled,
    /// A hotfix the CLI copied into `northbridge/request/`.
    File,
}

impl Source {
    pub const fn as_str(self) -> &'static str {
        match self {
            Source::Bundled => "bundled",
            Source::File => "file",
        }
    }
}

/// `northbridge/update-request.json`. Written by the CLI only, under its lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateRequest {
    pub generation: u64,
    #[serde(default)]
    pub force: bool,
    pub source: Source,
    pub requested_at: String,
}

impl UpdateRequest {
    pub fn load(store: &Store) -> Result<Option<UpdateRequest>> {
        let Some(raw) = store.read(Slot::NbUpdateRequest)? else {
            return Ok(None);
        };
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|e| fw_err(Kind::StorageError, format!("update-request.json: {e}")))
    }
}

/// The request the task has not answered yet: its generation is above both the state file's
/// `request_done_generation` and `handled`, what this process already answered (so a request
/// whose result could not be written is not run again in a loop).
pub fn pending_request(store: &Store, handled: u64) -> Option<UpdateRequest> {
    let request = UpdateRequest::load(store).ok()??;
    let done = StateFile::load(store)
        .map(|s| s.request_done_generation)
        .unwrap_or(0);
    (request.generation > done.max(handled)).then_some(request)
}

/// Resolve when a request newer than `handled` appears; polls every [`REQUEST_POLL`].
pub async fn wait_for_request(store: &Store, handled: u64) -> UpdateRequest {
    let mut tick = tokio::time::interval(REQUEST_POLL);
    loop {
        tick.tick().await;
        if let Some(r) = pending_request(store, handled) {
            return r;
        }
    }
}

/// What [`decide`] concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// No image to compare against: the image carries no Northbridge firmware and nothing
    /// was asked for. The automatic update is off.
    Off,
    /// The Northbridge runs the target image (or a hotfix kept over it): nothing to do.
    UpToDate { pinned: bool },
    /// A 180 B `GET_INFO`: no `app_valid` tail and no `ENTER_BOOTLOADER`. Only the §4
    /// J-Link migration helps; no attempt is made or counted.
    NeedsMigration,
    /// Flash the target. `nb_silent` when the Northbridge did not answer `GET_INFO`.
    Flash { nb_silent: bool },
    /// [`MAX_ATTEMPTS`] spent on this (image, boot): stop trying.
    BudgetExhausted { attempts: u32 },
}

/// The image a manual request asks for.
#[derive(Debug, Clone, Copy)]
pub struct Manual<'a> {
    pub force: bool,
    pub image: &'a Manifest,
}

fn same_image(app: &AppImageId, m: &Manifest) -> bool {
    app.image_len == m.image_len && app.crc32 == m.crc32
}

/// Whether to flash, and how — a pure function of what the probe heard, the bundled manifest,
/// the recorded state, this boot and a manual request (§3.4, §3.6).
///
/// The target is the request's image when there is a request, the bundled one otherwise.
pub fn decide(
    probe: Option<&ProbeInfo>,
    bundled: Option<&Manifest>,
    state: &StateFile,
    boot_id: &str,
    request: Option<&Manual<'_>>,
) -> Decision {
    let Some(target) = request.map(|r| r.image).or(bundled) else {
        return Decision::Off;
    };
    let force = request.is_some_and(|r| r.force);
    match probe {
        // A 180 B image: whatever the target, it cannot be entered from here.
        Some((_, None)) => return Decision::NeedsMigration,
        Some((_, Some(app))) if same_image(app, target) && !force => {
            return Decision::UpToDate { pinned: false };
        }
        // A hotfix kept over an unchanged bundle: the automatic compare leaves it.
        Some((_, Some(app))) if request.is_none() => {
            if let Some(p) = &state.pinned {
                if app.crc32 == p.crc32 && p.bundled_crc32 == Some(target.crc32) {
                    return Decision::UpToDate { pinned: true };
                }
            }
        }
        _ => {}
    }
    let key = BudgetKey {
        crc32: target.crc32,
        boot_id: boot_id.to_string(),
    };
    let attempts = state.attempts_for(&key);
    if attempts >= MAX_ATTEMPTS && !force {
        return Decision::BudgetExhausted { attempts };
    }
    Decision::Flash {
        nb_silent: probe.is_none(),
    }
}

/// What [`ensure`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// `None` when it stopped before deciding: no port, an unreadable request image.
    pub decision: Option<Decision>,
    /// The Northbridge runs the target image, or there was nothing to do.
    pub ok: bool,
    pub message: String,
}

/// Everything [`ensure`] takes from the system, injectable for tests.
#[derive(Debug, Clone)]
pub struct Env {
    pub bundle_dir: PathBuf,
    pub boot_id: String,
    pub opts: Options,
}

impl Env {
    /// The shipped values: [`bundle_dir`], the kernel's boot id, the flasher's defaults with
    /// [`VERIFY_ENV`] applied.
    pub fn from_system() -> Env {
        let verify = match std::env::var(VERIFY_ENV).as_deref().map(str::trim) {
            Ok("both") => VerifyMode::Both,
            Ok("readback") => VerifyMode::ReadBack,
            Ok("" | "checksum") | Err(_) => VerifyMode::Checksum,
            Ok(other) => {
                diag!("northbridge update: {VERIFY_ENV}={other:?} is not both|readback|checksum; using checksum");
                VerifyMode::Checksum
            }
        };
        Env {
            bundle_dir: bundle_dir(),
            boot_id: std::fs::read_to_string(BOOT_ID_PATH)
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string()),
            opts: Options {
                verify,
                ..Options::default()
            },
        }
    }
}

/// "The update is off" is said once per process, not at every restart of the task.
static OFF_LOGGED: AtomicBool = AtomicBool::new(false);

/// Bring the Northbridge on `uart_path` in line with the bundled image, or with `request`
/// (§3.4). Opens the port only when there is something to compare against, and closes it
/// before returning, so the caller can open the modem next.
///
/// Never fails the caller: a Northbridge that still answers HDLC keeps its old image and the
/// central starts on it; one that does not will fail the central's own open, which the
/// supervisor retries — and the next run lands here again, within the budget.
pub async fn ensure(
    uart_path: &str,
    store: &Store,
    status: &NorthbridgeStatus,
    request: Option<&UpdateRequest>,
) -> Report {
    let env = Env::from_system();
    ensure_with(
        || SerialFlashPort::open(uart_path),
        store,
        status,
        request,
        &env,
    )
    .await
}

/// [`ensure`] with the port opener and the system values injected.
pub async fn ensure_with<P, F>(
    open: F,
    store: &Store,
    status: &NorthbridgeStatus,
    request: Option<&UpdateRequest>,
    env: &Env,
) -> Report
where
    P: FlashPort,
    F: FnOnce() -> Result<P>,
{
    let mut state = match StateFile::load(store) {
        Ok(s) => s,
        Err(e) => {
            // Without a state file there is no budget: never flash blind.
            let message = format!("cannot read the update state: {}", e.message());
            diag!("northbridge update: {message}");
            return Report {
                decision: None,
                ok: false,
                message,
            };
        }
    };
    let bundle = match load_bundle(&env.bundle_dir) {
        Ok(b) => b,
        Err(e) => {
            // A bundle that does not match its own manifest is a build fault: off, said.
            diag!(
                "northbridge update: the bundled image is unusable, automatic update off: {}",
                e.message()
            );
            state.note = Some(format!("bundled image unusable: {}", e.message()));
            None
        }
    };
    let bundled_manifest = bundle.as_ref().map(|(_, m)| m);
    state.bundled = bundled_manifest.map(Manifest::version_string);

    let mut run = Run {
        store,
        status,
        state,
        request,
    };

    // The target: the request's image, or the bundled one.
    let target = match request {
        None => {
            match &bundle {
                Some(b) => b.clone(),
                None => {
                    if !OFF_LOGGED.swap(true, Ordering::Relaxed) {
                        diag!("northbridge update: no Northbridge firmware in {}: automatic update off",
                        env.bundle_dir.display());
                    }
                    status.set_update(run.state.note.is_some().then(|| run.state.summary()));
                    return Report {
                        decision: Some(Decision::Off),
                        ok: true,
                        message: "no Northbridge firmware in this image: automatic update off"
                            .to_string(),
                    };
                }
            }
        }
        Some(r) => {
            let loaded = match r.source {
                Source::Bundled => bundle.clone().ok_or_else(|| {
                    fw_err(
                        Kind::InvalidArgument,
                        "this image carries no Northbridge firmware: name one with --file",
                    )
                }),
                Source::File => load_files(
                    &store.path(Slot::NbRequestImage),
                    &store.path(Slot::NbRequestManifest),
                ),
            };
            match loaded {
                Ok(t) => t,
                Err(e) => return run.finish(None, false, e.message().to_string()),
            }
        }
    };
    let (image, manifest) = (&target.0, &target.1);
    let source = request.map_or(Source::Bundled, |r| r.source);

    let mut port = match open() {
        Ok(p) => p,
        Err(e) => {
            return run.finish(
                None,
                false,
                format!("cannot open the Northbridge UART: {}", e.message()),
            )
        }
    };
    let probed = match nb_flash::probe(&mut port, &env.opts).await {
        Ok(p) => p,
        Err(e) => {
            return run.finish(
                None,
                false,
                format!("GET_INFO before the update: {}", e.message()),
            )
        }
    };
    if let Some((info, app)) = &probed {
        diag!(
            "northbridge update: the Northbridge runs {}{}, the target is {} ({})",
            running(info),
            app.map_or(" (no app_valid tail)".to_string(), |a| format!(
                " ({} B, crc32 {:#010x})",
                a.image_len, a.crc32
            )),
            manifest.version_string(),
            source.as_str()
        );
    } else {
        diag!("northbridge update: the Northbridge does not answer GET_INFO");
    }

    let manual = request.map(|r| Manual {
        force: r.force,
        image: manifest,
    });
    let decision = decide(
        probed.as_ref(),
        bundled_manifest,
        &run.state,
        &env.boot_id,
        manual.as_ref(),
    );
    match decision {
        Decision::Off => run.finish(Some(decision), true, "nothing to compare against".into()),
        Decision::UpToDate { pinned } => {
            run.state.state = UpdateState::Idle;
            run.state.note = pinned.then(|| {
                format!(
                    "hotfix {} kept over the bundled image",
                    run.state
                        .pinned
                        .as_ref()
                        .map_or("?", |p| p.version.as_str())
                )
            });
            if !pinned {
                run.state.last_error = None;
                run.state.attempts = 0;
                run.state.pinned = pin_for(source, manifest, bundled_manifest);
            }
            // A pinned hotfix is what runs, not the bundled image `manifest` names.
            let message = match (pinned, run.state.pinned.as_ref()) {
                (true, Some(p)) => format!(
                    "the Northbridge keeps hotfix {} over the bundled {}",
                    p.version,
                    manifest.version_string()
                ),
                _ => format!("the Northbridge already runs {}", manifest.version_string()),
            };
            run.finish(Some(decision), true, message)
        }
        Decision::NeedsMigration => {
            let message = "the Northbridge image predates 0.4.0 (180 B GET_INFO, no \
                           ENTER_BOOTLOADER): it needs the one-time J-Link migration to \
                           nbboot + 0.4.0 (northbridge_update.md §4)"
                .to_string();
            run.state.state = UpdateState::Idle;
            run.state.note = Some(message.clone());
            run.finish(Some(decision), false, message)
        }
        Decision::BudgetExhausted { attempts } => {
            run.state.state = UpdateState::Failed;
            let message = format!(
                "{attempts} attempts to flash {} failed in this boot; not trying again until \
                 the Hub reboots or the bundle changes (northbridge-update --force overrides)",
                manifest.version_string()
            );
            run.finish(Some(decision), false, message)
        }
        Decision::Flash { nb_silent } => {
            let key = BudgetKey {
                crc32: manifest.crc32,
                boot_id: env.boot_id.clone(),
            };
            let attempt = run.state.attempts_for(&key) + 1;
            run.state.budget = Some(key);
            run.state.attempts = attempt;
            run.state.state = UpdateState::Flashing;
            run.state.last_attempt_at = Some(format_rfc3339(now_unix()));
            run.state.note = None;
            // Counted before a byte is sent: a crash mid-flash must still use up the budget.
            if let Err(e) = run.state.save(store) {
                return run.finish(
                    Some(decision),
                    false,
                    format!("cannot record the attempt, not flashing: {}", e.message()),
                );
            }
            status.set_update(Some(run.state.summary()));
            diag!(
                "northbridge update: flashing {} ({}), attempt {attempt} of {MAX_ATTEMPTS}{}",
                manifest.version_string(),
                source.as_str(),
                if nb_silent {
                    ", the Northbridge is silent"
                } else {
                    ""
                }
            );
            let opts = Options {
                nb_silent,
                ..env.opts
            };
            let mut log_step = journal;
            let result =
                nb_flash::update_northbridge(&mut port, image, manifest, &opts, &mut log_step)
                    .await;
            drop(port);
            match result {
                Ok(outcome) => {
                    let elapsed_ms = u64::try_from(outcome.elapsed.as_millis()).unwrap_or(0);
                    run.state.state = UpdateState::Idle;
                    run.state.attempts = 0;
                    run.state.last_error = None;
                    run.state.last_ok = Some(LastOk {
                        version: manifest.version_string(),
                        at: format_rfc3339(now_unix()),
                        elapsed_ms,
                    });
                    run.state.pinned = pin_for(source, manifest, bundled_manifest);
                    let message = format!(
                        "the Northbridge runs {} (flashed in {elapsed_ms} ms)",
                        manifest.version_string()
                    );
                    run.finish(Some(decision), true, message)
                }
                Err(e) => {
                    run.state.state = if attempt >= MAX_ATTEMPTS {
                        UpdateState::Failed
                    } else {
                        UpdateState::Idle
                    };
                    let message = format!(
                        "attempt {attempt} of {MAX_ATTEMPTS} to flash {} failed: {}",
                        manifest.version_string(),
                        e.message()
                    );
                    run.state.last_error = Some(message.clone());
                    run.finish(Some(decision), false, message)
                }
            }
        }
    }
}

/// The hotfix record after the Northbridge ended up on `image`: kept only for a `--file`
/// image that differs from the bundled one.
fn pin_for(source: Source, image: &Manifest, bundled: Option<&Manifest>) -> Option<Pinned> {
    let bundled_crc32 = bundled.map(|m| m.crc32);
    (source == Source::File && bundled_crc32 != Some(image.crc32)).then(|| Pinned {
        crc32: image.crc32,
        version: image.version_string(),
        bundled_crc32,
    })
}

/// The state being built up by one [`ensure_with`], and how it ends.
struct Run<'a> {
    store: &'a Store,
    status: &'a NorthbridgeStatus,
    state: StateFile,
    request: Option<&'a UpdateRequest>,
}

impl Run<'_> {
    /// Answer the request, save the state, publish the summary, log the outcome.
    fn finish(mut self, decision: Option<Decision>, ok: bool, message: String) -> Report {
        if let Some(r) = self.request {
            self.state.request_done_generation = r.generation;
            self.state.request_result = Some(RequestResult {
                generation: r.generation,
                ok,
                message: message.clone(),
            });
        } else if !ok && decision.is_none() {
            // An automatic run that could not even decide: shown, not counted.
            self.state.last_error = Some(message.clone());
        }
        if let Err(e) = self.state.save(self.store) {
            diag!(
                "northbridge update: cannot save the update state: {}",
                e.message()
            );
        }
        self.status.set_update(Some(self.state.summary()));
        diag!(
            "northbridge update: {} — {message}",
            if ok { "done" } else { "not done" }
        );
        Report {
            decision,
            ok,
            message,
        }
    }
}

/// One journal line per step (§3.5); the per-block progress is not journalled.
fn journal(p: &Progress) {
    match p {
        Progress::Started(step) => diag!("northbridge update: {} …", step.as_str()),
        Progress::Advanced { .. } => {}
        Progress::Note { step, message } => {
            diag!("northbridge update: {}: {message}", step.as_str())
        }
        Progress::Finished { step, elapsed } => diag!(
            "northbridge update: {} done in {} ms",
            step.as_str(),
            elapsed.as_millis()
        ),
        Progress::Failed { step, error } => {
            diag!("northbridge update: {} failed: {error}", step.as_str())
        }
    }
}

fn running(info: &ModemInfo) -> String {
    format!(
        "{}.{}.{}+{}",
        info.fw[0],
        info.fw[1],
        info.fw[2],
        info.git_sha
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

// ---------------------------------------------------------------------------------------------
// The CLI half: `proximosctl control.radio northbridge-update`.

/// Write a request (and, for `--file`, its image) for the running P2P task; returns its
/// generation. The generation is one above both the last request and the last one answered,
/// under the request's lock, so two CLIs cannot hand out the same one.
pub fn submit_request(
    store: &Store,
    force: bool,
    hotfix: Option<(&[u8], &[u8])>,
) -> Result<UpdateRequest> {
    let _lock = store.lock(Slot::NbUpdateRequest)?;
    let prev = UpdateRequest::load(store)
        .ok()
        .flatten()
        .map_or(0, |r| r.generation);
    let done = StateFile::load(store)?.request_done_generation;
    let source = match hotfix {
        Some((image, manifest)) => {
            store.write_bytes(Slot::NbRequestImage, image)?;
            store.write_bytes(Slot::NbRequestManifest, manifest)?;
            Source::File
        }
        None => Source::Bundled,
    };
    let request = UpdateRequest {
        generation: prev.max(done) + 1,
        force,
        source,
        requested_at: format_rfc3339(now_unix()),
    };
    let text = serde_json::to_string_pretty(&request)
        .map_err(|e| fw_err(Kind::StorageError, format!("update-request.json: {e}")))?;
    // Last: the task reads the image only after it sees the new generation.
    store.write(Slot::NbUpdateRequest, &(text + "\n"))?;
    Ok(request)
}

/// Wait until the task has answered `generation`, polling the state file every `poll`;
/// `Timeout` (exit 21) after `timeout`.
pub async fn wait_result(
    store: &Store,
    generation: u64,
    timeout: Duration,
    poll: Duration,
) -> Result<RequestResult> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(state) = StateFile::load(store) {
            if state.request_done_generation >= generation {
                return Ok(match state.request_result {
                    Some(r) if r.generation == generation => r,
                    // A later request was answered first: this one was superseded.
                    _ => RequestResult {
                        generation,
                        ok: false,
                        message: format!(
                            "request {generation} was superseded by request {} before it ran",
                            state.request_done_generation
                        ),
                    },
                });
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(fw_err(
                Kind::Timeout,
                format!(
                    "the P2P task did not answer northbridge-update request {generation} \
                     within {} s; it may still run — see `status --json` (northbridge.update)",
                    timeout.as_secs()
                ),
            ));
        }
        tokio::time::sleep(poll).await;
    }
}

/// `northbridge-update` as the CLI parsed it.
#[derive(Debug, Clone, Default)]
pub struct CliRequest {
    /// `--file <bin> --manifest <json>`: a hotfix instead of the bundled image.
    pub hotfix: Option<(PathBuf, PathBuf)>,
    pub force: bool,
    /// How long to wait for the task's answer; `None` for `--no-wait`.
    pub wait: Option<Duration>,
}

/// What [`request_update`] did: the request, the image it names, and the task's answer
/// (`None` with `--no-wait`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOutcome {
    pub request: UpdateRequest,
    pub image: String,
    pub result: Option<RequestResult>,
}

/// The whole CLI arm: check the image, check that a P2P task is there to answer, submit the
/// request and wait for its result.
///
/// The image is checked here, before anything is written, so a bad `--file` is the
/// operator's `InvalidArgument` (exit 2) rather than a failed attempt in the task. `task` is
/// the run document the P2P task's health loop mirrors (`/run/proximos/radio/
/// northbridge.json`): absent, nothing would answer (`DaemonDown`, exit 23); `disabled`, this
/// Hub's mode runs no P2P task (`P2pUnavailable`, exit 32). The heartbeat is not consulted on
/// purpose — it is withheld while the Northbridge is silent, which is exactly when an update
/// may be needed. A failed update is `Transport` (exit 22) with the task's message.
pub async fn request_update(
    store: &Store,
    req: &CliRequest,
    bundle: &Path,
    task: Option<&serde_json::Value>,
    poll: Duration,
) -> Result<CliOutcome> {
    let hotfix = match &req.hotfix {
        Some((bin, json)) => {
            let (image, m) = load_files(bin, json)?;
            let raw = std::fs::read(json).map_err(|e| {
                fw_err(
                    Kind::InvalidArgument,
                    format!("cannot read {}: {e}", json.display()),
                )
            })?;
            Some((image, raw, m))
        }
        None => None,
    };
    let image = match &hotfix {
        Some((_, _, m)) => m.version_string(),
        None => match load_bundle(bundle)? {
            Some((_, m)) => m.version_string(),
            None => {
                return Err(fw_err(
                    Kind::InvalidArgument,
                    format!(
                        "this image carries no Northbridge firmware ({} is empty): give \
                         --file <northbridge.bin> --manifest <northbridge.json>",
                        bundle.display()
                    ),
                ))
            }
        },
    };
    match task.and_then(|d| d.get("state")).and_then(|s| s.as_str()) {
        None => {
            return Err(fw_err(
                Kind::DaemonDown,
                "no P2P task to run the update: proximos-control has not published \
                 /run/proximos/radio/northbridge.json",
            ))
        }
        // The idle Northbridge task (#158) answers on a Hub without P2P.
        Some("disabled")
            if task.and_then(|d| d.get("nb_task")).and_then(|v| v.as_str()) == Some("idle") => {}
        Some("disabled") => {
            return Err(fw_err(
                Kind::P2pUnavailable,
                "the P2P task is disabled on this Hub: nothing drives the Northbridge",
            ))
        }
        Some(_) => {}
    }
    let request = submit_request(
        store,
        req.force,
        hotfix
            .as_ref()
            .map(|(image, json, _)| (image.as_slice(), json.as_slice())),
    )?;
    let result = match req.wait {
        None => None,
        Some(timeout) => {
            let r = wait_result(store, request.generation, timeout, poll).await?;
            if !r.ok {
                return Err(fw_err(Kind::Transport, r.message));
            }
            Some(r)
        }
    };
    Ok(CliOutcome {
        request,
        image,
        result,
    })
}

#[cfg(test)]
mod tests;
