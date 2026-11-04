//! Deep standby — what "powered down" has to mean on a board that cannot be
//! woken.
//!
//! A Viewer-initiated power-off used to run `systemctl poweroff`, which puts the
//! BCM2711 into a halt state. The battery keeps the rails up, so the CM4 sits
//! latched there and *nothing on the board can bring it back*: the southbridge's
//! VIN is an on-demand ADC read with no comparator or interrupt behind it, every
//! southbridge pin is an already-assigned output, the northbridge configures
//! four pins in total (UART + SWD), the RTC overlay is loaded without
//! `wakeup-source`, and the reset line runs CM4 → southbridge rather than the
//! reverse. Plugging PoE back in did nothing; the device stayed unreachable
//! until someone pulled the battery.
//!
//! With no wake signal to build on, the only mechanism left is to never enter
//! that halt. "Powered down" becomes a state the agent stays alive in — panel
//! dark, sensor rails down, silent, not pairable — while the power monitor keeps
//! reading VIN. The DC-connect edge it already detects becomes the wake trigger.
//!
//! Two pieces live here:
//!
//! * [`StandbySignal`], the in-process state every monitor loop consults to
//!   decide whether to skip its tick. Modelled on
//!   [`crate::libs::display::blank`] — process-wide static for the same reason
//!   (the MQTT command executor is thirteen handles deep already, and there is
//!   exactly one of these per process), wrapped in a struct so tests get their
//!   own instance instead of racing each other through the static.
//! * [`StandbyMarker`], the on-disk record that survives the agent dying. The
//!   unit runs with `Restart=on-failure`, so without it a panic in standby would
//!   bring the agent back up in ten seconds and silently resume patient
//!   monitoring on a device the operator believes is off.
//!
//! The marker is written next to the medical database rather than in `/tmp`:
//! `PrivateTmp=true` gives the unit a private tmpfs that does not survive a
//! restart, which is exactly the event the marker exists to survive.
//!
//! Two properties it has to hold:
//!
//! * **Atomic.** Temp file → fsync → rename → fsync the parent directory. This
//!   file is read on the boot that follows a power interruption, so a torn write
//!   must leave either the old marker or the new one, never half of one.
//! * **Never fatal.** Missing, truncated or unparseable degrades to "boot awake"
//!   plus a WARN. A device that cannot read its own marker must still boot and
//!   monitor patients.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::libs::config::StandbyConfig;

/// Normal operation.
pub const AWAKE: u8 = 0;
/// Powered down as far as this hardware allows: dark, silent, waiting for VIN.
pub const STANDBY: u8 = 1;

/// File name written next to the medical database on the persistent partition.
const MARKER_FILE_NAME: &str = "standby.json";

/// Where the marker goes when the storage config gives us nothing usable.
const FALLBACK_MARKER_DIR: &str = "/data/fiber";

/// Whether the device is in standby, shared across every monitor thread.
///
/// A struct rather than bare statics so the transitions can be unit-tested on a
/// local instance — tests in the same binary run in parallel and would otherwise
/// race each other through the process-wide [`STATE`].
pub struct StandbySignal {
    state: AtomicU8,
}

impl Default for StandbySignal {
    fn default() -> Self {
        Self::new()
    }
}

impl StandbySignal {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(AWAKE),
        }
    }

    /// Enter standby. Returns whether this call was the one that changed the
    /// state, so a caller can tell a fresh entry from a repeat and skip the
    /// hardware teardown the second time.
    pub fn request(&self) -> bool {
        self.state
            .compare_exchange(AWAKE, STANDBY, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Leave standby. Returns whether this call was the one that changed the
    /// state — the resume path does real work (rails up, panel back, audit row)
    /// that must not run twice if two threads spot the DC edge together.
    pub fn resume(&self) -> bool {
        self.state
            .compare_exchange(STANDBY, AWAKE, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::SeqCst)
    }

    pub fn is_standby(&self) -> bool {
        self.state() == STANDBY
    }
}

/// The one standby state in this process.
static STATE: StandbySignal = StandbySignal::new();

/// Enter standby. See [`StandbySignal::request`].
pub fn request_standby() -> bool {
    STATE.request()
}

/// Leave standby. See [`StandbySignal::resume`].
pub fn resume() -> bool {
    STATE.resume()
}

/// Whether the device is currently in standby. Consulted at the top of every
/// monitor loop tick.
pub fn is_standby() -> bool {
    STATE.is_standby()
}

/// Current state: [`AWAKE`] or [`STANDBY`].
pub fn state() -> u8 {
    STATE.state()
}

/// What asked the device to wake, for the audit row and the MQTT event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeReason {
    /// A confirmed, newly arrived DC supply.
    DcPresent,
    /// A local button hold — the escape hatch for a device whose supply cannot be
    /// detected, so it can never be stranded dark.
    Button,
}

impl WakeReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            WakeReason::DcPresent => "dc_present",
            WakeReason::Button => "button",
        }
    }
}

/// Wake requested from somewhere other than the VIN watch. `1` = button.
///
/// A flag rather than a direct call because the requester is the button thread,
/// which owns no hardware: every standby hardware transition stays in
/// `PowerMonitor`, which already holds the STM bridge, the buzzer and the storage
/// handle. `PowerMonitor` picks this up on its next poll.
static WAKE_REQUEST: AtomicU8 = AtomicU8::new(0);

const WAKE_NONE: u8 = 0;
const WAKE_BUTTON: u8 = 1;

/// Ask the device to leave standby. Ignored when it is already awake.
pub fn request_wake(reason: WakeReason) {
    if !is_standby() {
        return;
    }
    let code = match reason {
        WakeReason::Button => WAKE_BUTTON,
        // The VIN watch resumes directly; it has no need of the flag.
        WakeReason::DcPresent => return,
    };
    WAKE_REQUEST.store(code, Ordering::SeqCst);
    eprintln!("[standby] Wake requested ({})", reason.as_str());
}

/// Consume a pending wake request, if any.
pub fn take_wake_request() -> Option<WakeReason> {
    match WAKE_REQUEST.swap(WAKE_NONE, Ordering::SeqCst) {
        WAKE_BUTTON => Some(WakeReason::Button),
        _ => None,
    }
}

/// Drop any pending wake request — used on entry so a stale press cannot wake the
/// device the instant it goes down.
pub fn clear_wake_request() {
    WAKE_REQUEST.store(WAKE_NONE, Ordering::SeqCst);
}

/// The on-disk record that the device was put into standby deliberately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandbyMarker {
    /// Unix seconds. Stored as an integer rather than a formatted string so a
    /// wrong or unset system clock cannot make the file unparseable — the RTC is
    /// only synced by a separate oneshot unit at boot.
    pub entered_at_unix: u64,
    /// The reason carried by the signed power-off command.
    pub reason: String,
    /// Who signed it. Kept so the resume audit row can name the same operator.
    pub requested_by: String,
}

impl StandbyMarker {
    /// Build a marker stamped with the current wall clock.
    pub fn new(reason: String, requested_by: String) -> Self {
        Self {
            entered_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs(),
            reason,
            requested_by,
        }
    }

    /// When standby was entered, as an RFC 3339 string for MQTT and `fiberctl`.
    pub fn entered_at_rfc3339(&self) -> String {
        chrono::DateTime::from_timestamp(self.entered_at_unix as i64, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| format!("@{}", self.entered_at_unix))
    }

    /// Write the marker into `dir`, atomically.
    ///
    /// Errors are returned rather than logged so the caller can abort the
    /// standby entry: a standby the next boot cannot detect is worse than no
    /// standby at all, because the device would come back up monitoring while
    /// reporting itself off.
    pub fn write(&self, dir: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        fs::create_dir_all(dir)?;
        let final_path = dir.join(MARKER_FILE_NAME);
        let temp_path = dir.join(format!("{MARKER_FILE_NAME}.tmp"));

        {
            let mut f = fs::File::create(&temp_path)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }

        fs::rename(&temp_path, &final_path)?;

        // The rename is only durable once the directory entry is. Without this
        // the marker can vanish on a power cut that lands between the two.
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }

        Ok(())
    }

    /// Read the marker from `dir`, or `None` if there is nothing usable there.
    ///
    /// Never fails: a missing file is the normal case, and a corrupt one must
    /// not stop the device booting.
    pub fn read(dir: &Path) -> Option<Self> {
        let path = dir.join(MARKER_FILE_NAME);
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(e) => {
                eprintln!("[standby] WARN: cannot read {}: {e}", path.display());
                return None;
            }
        };

        match serde_json::from_slice(&raw) {
            Ok(marker) => Some(marker),
            Err(e) => {
                eprintln!(
                    "[standby] WARN: {} is unreadable ({e}) — treating as absent",
                    path.display()
                );
                None
            }
        }
    }

    /// Remove the marker. Best-effort: a leftover marker only matters on a boot
    /// with no DC power, and [`boot_decision`] treats DC power as the winner.
    pub fn clear(dir: &Path) {
        let path = dir.join(MARKER_FILE_NAME);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("[standby] WARN: cannot clear {}: {e}", path.display()),
        }
    }
}

/// Process-wide settings, published once from `main` so the MQTT command
/// executor can reach them.
///
/// That executor already takes fourteen parameters; the marker path and the
/// governor name are boot-time constants, so threading two more handles down to
/// it would cost more clarity than it buys. Set once and never mutated, which is
/// what makes a `OnceLock` honest here rather than ambient state.
struct StandbySettings {
    marker_dir: PathBuf,
    config: StandbyConfig,
}

static SETTINGS: OnceLock<StandbySettings> = OnceLock::new();

/// Publish the standby settings. Called once, early in `main`.
pub fn init(marker_dir: PathBuf, config: StandbyConfig) {
    if SETTINGS
        .set(StandbySettings { marker_dir, config })
        .is_err()
    {
        eprintln!("[standby] WARN: settings already initialised — ignoring second init");
    }
}

/// Where the marker lives. Falls back to the data partition if `init` never ran,
/// so a code path that forgets to initialise still writes somewhere persistent
/// rather than into the process's working directory.
pub fn configured_marker_dir() -> PathBuf {
    SETTINGS
        .get()
        .map(|s| s.marker_dir.clone())
        .unwrap_or_else(|| PathBuf::from(FALLBACK_MARKER_DIR))
}

/// The standby settings, or defaults if `init` never ran.
pub fn config() -> StandbyConfig {
    SETTINGS.get().map(|s| s.config.clone()).unwrap_or_default()
}

/// Directory the marker lives in, derived from the configured database path so
/// it lands on the same persistent partition.
pub fn marker_dir(db_path: &str) -> PathBuf {
    Path::new(db_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(FALLBACK_MARKER_DIR))
}

/// Root of the cpufreq policies. One directory per policy; the CM4's four cores
/// share `policy0`, but the loop does not assume that.
const CPUFREQ_ROOT: &str = "/sys/devices/system/cpu/cpufreq";

/// The governor in force before standby, so resume can put it back rather than
/// guessing a name.
static SAVED_GOVERNOR: OnceLock<Option<String>> = OnceLock::new();

fn governor_policies() -> Vec<PathBuf> {
    match fs::read_dir(CPUFREQ_ROOT) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.join("scaling_governor").is_file())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Switch every cpufreq policy to `name`, remembering what was there first.
///
/// Best-effort throughout: `fiber.service` runs with `ProtectSystem=strict`, and
/// a kernel without cpufreq exposed has no policies at all. Neither is a reason
/// to refuse a power-off, so failures are logged and ignored — the governor is an
/// optimisation, not part of the standby contract.
pub fn apply_cpu_governor(name: &str) {
    if name.is_empty() {
        return;
    }
    let policies = governor_policies();
    if policies.is_empty() {
        eprintln!("[standby] no cpufreq policies under {CPUFREQ_ROOT} — leaving CPU as-is");
        return;
    }

    let _ = SAVED_GOVERNOR.set(
        fs::read_to_string(policies[0].join("scaling_governor"))
            .ok()
            .map(|s| s.trim().to_string()),
    );

    for policy in policies {
        let path = policy.join("scaling_governor");
        if let Err(e) = fs::write(&path, name) {
            eprintln!(
                "[standby] WARN: cannot set governor via {}: {e}",
                path.display()
            );
        }
    }
}

/// Put the governor back to whatever [`apply_cpu_governor`] found. No-op if it
/// never ran or could not read the original.
pub fn restore_cpu_governor() {
    let Some(Some(saved)) = SAVED_GOVERNOR.get() else {
        return;
    };
    for policy in governor_policies() {
        let path = policy.join("scaling_governor");
        if let Err(e) = fs::write(&path, saved) {
            eprintln!(
                "[standby] WARN: cannot restore governor via {}: {e}",
                path.display()
            );
        }
    }
}

/// What to do with the marker we found at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootDecision {
    /// Boot normally and clear any marker.
    Awake,
    /// Come back up in standby without re-running the audit-and-flush entry
    /// path: the operator asked for this device to be off and nothing has
    /// changed that.
    ReenterStandby,
}

/// Decide how to boot, given the marker and the first VIN reading.
///
/// Pure so the whole table can be tested without hardware.
///
/// DC power wins over the marker. That covers the case where the battery gave
/// out completely while in standby: the rails dropped, the CM4 cold-booted when
/// PoE came back, and the stale marker must not put a mains-powered device
/// straight back to sleep. It also means an operator can always recover a device
/// by giving it power.
///
/// An unknown VIN — the ADC read failed — counts as "not DC". Coming up in
/// standby when we cannot prove power is present is the conservative choice: the
/// device stays visibly off rather than silently resuming clinical measurement,
/// and the next successful read wakes it a few seconds later.
pub fn boot_decision(
    marker_present: bool,
    vin_mv: Option<u16>,
    detection_threshold_mv: u16,
) -> BootDecision {
    if !marker_present {
        return BootDecision::Awake;
    }
    match vin_mv {
        Some(mv) if mv >= detection_threshold_mv => BootDecision::Awake,
        _ => BootDecision::ReenterStandby,
    }
}

/// Counts consecutive above-threshold VIN readings, so a cable being wiggled
/// cannot thrash a device in and out of standby.
///
/// Each resume does real work — sensor rails up, panel back, an audit row, an
/// MQTT event — and the entry path costs a database flush, so a flapping supply
/// must not drive that loop at the poll rate.
#[derive(Debug, Clone)]
pub struct DcConfirm {
    needed: u32,
    seen: u32,
}

impl DcConfirm {
    /// `needed` is clamped to at least 1: zero confirmations would mean firing
    /// before any reading had been taken.
    pub fn new(needed: u32) -> Self {
        Self {
            needed: needed.max(1),
            seen: 0,
        }
    }

    /// Feed one reading. Returns true on the reading that completes an
    /// unbroken run of `needed` above-threshold samples, and not again until a
    /// below-threshold sample breaks the run.
    pub fn observe(&mut self, above_threshold: bool) -> bool {
        if !above_threshold {
            self.seen = 0;
            return false;
        }
        // Saturating so a long run cannot wrap around and re-fire.
        self.seen = self.seen.saturating_add(1);
        self.seen == self.needed
    }

    pub fn reset(&mut self) {
        self.seen = 0;
    }

    /// Consecutive above-threshold samples collected so far.
    pub fn progress(&self) -> u32 {
        self.seen
    }
}

/// What convinced the watch that the supply had actually been interrupted.
///
/// Recorded so the journal can say *why* a device woke — the two sources carry
/// different confidence, and telling them apart after the fact was impossible in
/// the first version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmEvidence {
    /// Standby began with no DC present, so any arrival is an arrival.
    EnteredOnBattery,
    /// A VIN reading below the threshold was actually sampled. Strongest: proves
    /// the *power* went away.
    VinDip,
    /// The Ethernet carrier counter moved. Proves the *link* was interrupted,
    /// which a poll cannot miss however brief it was — but a switch-side bounce
    /// can produce it without power ever dropping.
    LinkDown,
}

/// Watches VIN during standby and reports when PoE has *newly* arrived.
///
/// Level-triggering would be wrong. An operator can power off a device that
/// still has PoE plugged in — decommissioning it, or moving it — and a bare
/// "is DC present?" test would resume it on the very next poll, making the
/// power-off look broken. The device must first have evidence that DC went
/// *away*; only then is a return to DC an arrival.
///
/// The first version took that evidence solely from sampling VIN below the
/// threshold, which is what made it fail in the field: at a 5 s poll a quick
/// unplug/replug fell entirely between two samples, so the absence was never
/// observed, the watch never armed, and the device stayed dark until someone
/// held the cable out for longer than the interval. Arming now also accepts the
/// Ethernet carrier counter, which is monotonic and therefore cannot miss a
/// transition however short — see [`super::link::CarrierWatch`].
///
/// Confirmation is unchanged and still requires DC actually present for
/// `confirm_polls` consecutive samples, so link evidence alone can never wake a
/// device that has no power.
#[derive(Debug, Clone)]
pub struct ResumeWatch {
    confirm: DcConfirm,
    evidence: Option<ArmEvidence>,
}

impl ResumeWatch {
    /// `dc_present_at_entry` is what VIN said when standby began. If DC was
    /// already there, the watch starts unarmed and waits for evidence that the
    /// supply went away before it will treat one as newly connected.
    pub fn new(confirm_polls: u32, dc_present_at_entry: bool) -> Self {
        Self {
            confirm: DcConfirm::new(confirm_polls),
            evidence: (!dc_present_at_entry).then_some(ArmEvidence::EnteredOnBattery),
        }
    }

    /// Whether the watch has evidence of an interruption, so an arrival counts.
    pub fn is_armed(&self) -> bool {
        self.evidence.is_some()
    }

    /// What armed it, or `None` while still unarmed.
    pub fn evidence(&self) -> Option<ArmEvidence> {
        self.evidence
    }

    /// How many consecutive DC-present samples have been collected.
    pub fn confirm_progress(&self) -> u32 {
        self.confirm.progress()
    }

    /// Discard progress towards a confirmation without arming.
    ///
    /// For a stale or failed reading, which is evidence of nothing: treating it as
    /// "DC absent" would arm a watch that deliberately started unarmed, and a
    /// device put into standby on mains would then wake on its next good reading.
    pub fn break_run(&mut self) {
        self.confirm.reset();
    }

    /// Feed one poll's worth of evidence.
    ///
    /// `dc_present` is the current, *freshly read* DC state; `link_bounced` is
    /// whether the carrier counter has moved since standby began. Returns true on
    /// the poll that completes a newly arrived, debounced DC connection.
    pub fn observe(&mut self, dc_present: bool, link_bounced: bool) -> bool {
        if !dc_present {
            // Sampling the absence directly is the strongest evidence, and it
            // outranks a link bounce that may not have involved power at all.
            self.evidence = Some(ArmEvidence::VinDip);
            self.confirm.reset();
            return false;
        }
        if self.evidence.is_none() && link_bounced {
            self.evidence = Some(ArmEvidence::LinkDown);
        }
        if self.evidence.is_none() {
            // DC has been present the whole time and the cable was never
            // disturbed — not an arrival.
            return false;
        }
        self.confirm.observe(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // --- StandbySignal -----------------------------------------------------

    #[test]
    fn starts_awake() {
        assert_eq!(StandbySignal::new().state(), AWAKE);
        assert!(!StandbySignal::new().is_standby());
    }

    #[test]
    fn request_then_resume_round_trips() {
        let signal = StandbySignal::new();

        assert!(signal.request());
        assert!(signal.is_standby());

        assert!(signal.resume());
        assert!(!signal.is_standby());
    }

    #[test]
    fn only_the_first_request_reports_the_transition() {
        // The second caller must not repeat the hardware teardown.
        let signal = StandbySignal::new();
        assert!(signal.request());
        assert!(!signal.request());
        assert!(signal.is_standby());
    }

    #[test]
    fn only_the_first_resume_reports_the_transition() {
        // Two threads can spot the same DC edge; the audit row and the rails
        // must only come up once.
        let signal = StandbySignal::new();
        signal.request();
        assert!(signal.resume());
        assert!(!signal.resume());
        assert!(!signal.is_standby());
    }

    #[test]
    fn resume_while_awake_is_a_no_op() {
        let signal = StandbySignal::new();
        assert!(!signal.resume());
        assert_eq!(signal.state(), AWAKE);
    }

    // --- StandbyMarker ----------------------------------------------------

    #[test]
    fn marker_round_trips_through_the_filesystem() {
        let dir = TempDir::new().unwrap();
        let marker = StandbyMarker::new("bench test".into(), "matej".into());

        marker.write(dir.path()).unwrap();
        let read_back = StandbyMarker::read(dir.path()).expect("marker should be readable");

        assert_eq!(read_back, marker);
    }

    #[test]
    fn an_absent_marker_reads_as_none() {
        let dir = TempDir::new().unwrap();
        assert!(StandbyMarker::read(dir.path()).is_none());
    }

    #[test]
    fn an_unparseable_marker_reads_as_none() {
        // A device that cannot read its own marker must still boot.
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(MARKER_FILE_NAME), b"{ truncated").unwrap();

        assert!(StandbyMarker::read(dir.path()).is_none());
    }

    #[test]
    fn writing_twice_leaves_one_marker_and_no_temp_file() {
        let dir = TempDir::new().unwrap();
        StandbyMarker::new("first".into(), "a".into())
            .write(dir.path())
            .unwrap();
        let second = StandbyMarker::new("second".into(), "b".into());
        second.write(dir.path()).unwrap();

        assert_eq!(StandbyMarker::read(dir.path()).unwrap(), second);
        assert!(
            !dir.path().join(format!("{MARKER_FILE_NAME}.tmp")).exists(),
            "the temp file must be renamed away, not left behind"
        );
    }

    #[test]
    fn clearing_is_idempotent() {
        let dir = TempDir::new().unwrap();
        StandbyMarker::new("r".into(), "b".into())
            .write(dir.path())
            .unwrap();

        StandbyMarker::clear(dir.path());
        assert!(StandbyMarker::read(dir.path()).is_none());

        // Clearing an already-clear directory must not complain.
        StandbyMarker::clear(dir.path());
        assert!(StandbyMarker::read(dir.path()).is_none());
    }

    #[test]
    fn write_creates_a_missing_directory() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("data").join("fiber");

        StandbyMarker::new("r".into(), "b".into())
            .write(&nested)
            .unwrap();
        assert!(StandbyMarker::read(&nested).is_some());
    }

    #[test]
    fn entered_at_formats_as_rfc3339() {
        let marker = StandbyMarker {
            entered_at_unix: 0,
            reason: "r".into(),
            requested_by: "b".into(),
        };
        assert!(marker
            .entered_at_rfc3339()
            .starts_with("1970-01-01T00:00:00"));
    }

    // --- marker_dir -------------------------------------------------------

    #[test]
    fn marker_sits_beside_the_database() {
        assert_eq!(
            marker_dir("/data/fiber/fiber_medical.db"),
            PathBuf::from("/data/fiber")
        );
    }

    #[test]
    fn a_bare_database_filename_falls_back_to_the_data_partition() {
        // Path::parent() gives Some("") for a bare filename, which would put the
        // marker in the process's cwd — wrong partition, and not writable under
        // ProtectSystem=strict.
        assert_eq!(
            marker_dir("fiber_medical.db"),
            PathBuf::from(FALLBACK_MARKER_DIR)
        );
    }

    // --- boot_decision ----------------------------------------------------

    #[test]
    fn boot_decision_table() {
        const T: u16 = 12000;
        let cases = [
            // (marker, vin, expected, why)
            (
                false,
                Some(15000),
                BootDecision::Awake,
                "no marker, on mains",
            ),
            (false, Some(0), BootDecision::Awake, "no marker, no power"),
            (false, None, BootDecision::Awake, "no marker, ADC silent"),
            (
                true,
                Some(15000),
                BootDecision::Awake,
                "battery died in standby and PoE cold-booted us: DC wins",
            ),
            (
                true,
                Some(T),
                BootDecision::Awake,
                "exactly at the threshold counts as DC",
            ),
            (
                true,
                Some(T - 1),
                BootDecision::ReenterStandby,
                "just below the threshold is still standby",
            ),
            (
                true,
                Some(0),
                BootDecision::ReenterStandby,
                "crash-restart on battery must not resume monitoring",
            ),
            (
                true,
                None,
                BootDecision::ReenterStandby,
                "cannot prove power is present, so stay off",
            ),
        ];

        for (marker, vin, expected, why) in cases {
            assert_eq!(boot_decision(marker, vin, T), expected, "{why}");
        }
    }

    // --- DcConfirm --------------------------------------------------------

    #[test]
    fn confirm_fires_once_the_run_is_long_enough() {
        let mut c = DcConfirm::new(2);
        assert!(!c.observe(true), "one sample is not a run of two");
        assert!(c.observe(true));
    }

    #[test]
    fn confirm_does_not_fire_again_on_a_continuing_run() {
        // Otherwise every subsequent poll would re-run the resume.
        let mut c = DcConfirm::new(2);
        c.observe(true);
        assert!(c.observe(true));
        for _ in 0..10 {
            assert!(!c.observe(true));
        }
    }

    #[test]
    fn a_below_threshold_sample_breaks_the_run() {
        let mut c = DcConfirm::new(3);
        assert!(!c.observe(true));
        assert!(!c.observe(true));
        assert!(!c.observe(false), "the flap resets progress");
        assert!(!c.observe(true));
        assert!(!c.observe(true));
        assert!(c.observe(true), "a fresh run of three fires");
    }

    #[test]
    fn a_flapping_supply_never_confirms() {
        let mut c = DcConfirm::new(2);
        for _ in 0..20 {
            assert!(!c.observe(true));
            assert!(!c.observe(false));
        }
    }

    #[test]
    fn zero_confirmations_is_clamped_to_one() {
        let mut c = DcConfirm::new(0);
        assert!(c.observe(true), "still needs one actual reading");
    }

    #[test]
    fn reset_clears_progress() {
        let mut c = DcConfirm::new(2);
        c.observe(true);
        c.reset();
        assert!(!c.observe(true), "progress was dropped");
        assert!(c.observe(true));
    }

    // --- ResumeWatch ------------------------------------------------------

    #[test]
    fn entering_standby_on_battery_arms_immediately() {
        let mut w = ResumeWatch::new(1, false);
        assert!(w.is_armed());
        assert_eq!(w.evidence(), Some(ArmEvidence::EnteredOnBattery));
        assert!(w.observe(true, false), "PoE arriving is an arrival");
    }

    #[test]
    fn entering_standby_on_mains_does_not_resume_on_the_next_poll() {
        // Powering off a device that still has PoE plugged in must leave it off,
        // not bounce it straight back up.
        let mut w = ResumeWatch::new(1, true);
        assert!(!w.is_armed());
        for _ in 0..50 {
            assert!(
                !w.observe(true, false),
                "DC was never absent and the cable was never disturbed"
            );
        }
    }

    #[test]
    fn unplugging_then_replugging_resumes_a_device_that_slept_on_mains() {
        let mut w = ResumeWatch::new(1, true);
        assert!(!w.observe(true, false));
        assert!(!w.observe(false, false), "cable pulled — now armed");
        assert_eq!(w.evidence(), Some(ArmEvidence::VinDip));
        assert!(w.observe(true, false), "cable back — a genuine arrival");
    }

    #[test]
    fn a_replug_too_quick_to_sample_still_wakes_the_device() {
        // THE reported bug. Standby entered on mains; the unplug/replug happened
        // entirely between two polls, so VIN reads present on every sample and the
        // dip is never seen. Only the carrier counter remembers it.
        let mut w = ResumeWatch::new(2, true);

        assert!(!w.observe(true, false), "before the cable was touched");
        assert!(!w.is_armed());

        // Next poll: VIN still reads present, but carrier has bounced.
        assert!(!w.observe(true, true), "armed, first confirmation");
        assert_eq!(w.evidence(), Some(ArmEvidence::LinkDown));
        assert!(
            w.observe(true, true),
            "second confirmation fires the resume"
        );
    }

    #[test]
    fn a_sampled_dip_outranks_a_link_bounce_as_evidence() {
        // Both can be true at once; the VIN dip is the one that proves the power
        // actually went away, so that is what gets logged.
        let mut w = ResumeWatch::new(1, true);
        w.observe(true, true);
        assert_eq!(w.evidence(), Some(ArmEvidence::LinkDown));

        w.observe(false, true);
        assert_eq!(w.evidence(), Some(ArmEvidence::VinDip));
    }

    #[test]
    fn a_link_bounce_alone_cannot_wake_an_unpowered_device() {
        // Carrier evidence only ever arms. Confirmation still needs DC actually
        // present, so a bounce with no power cannot bring the device up.
        let mut w = ResumeWatch::new(1, true);
        for _ in 0..20 {
            assert!(!w.observe(false, true));
        }
        assert!(w.is_armed());
        assert!(
            w.observe(true, true),
            "and it wakes once power is really back"
        );
    }

    #[test]
    fn resume_watch_still_debounces_once_armed() {
        let mut w = ResumeWatch::new(3, false);
        assert!(!w.observe(true, false));
        assert!(!w.observe(true, false));
        assert!(
            !w.observe(false, false),
            "flap resets the run but keeps it armed"
        );
        assert!(w.is_armed());
        assert!(!w.observe(true, false));
        assert!(!w.observe(true, false));
        assert!(w.observe(true, false));
    }

    #[test]
    fn resume_watch_fires_only_once_per_arrival() {
        let mut w = ResumeWatch::new(2, false);
        w.observe(true, false);
        assert!(w.observe(true, false));
        for _ in 0..10 {
            assert!(
                !w.observe(true, false),
                "the resume must not re-fire every poll"
            );
        }
    }

    #[test]
    fn break_run_drops_progress_without_arming() {
        // What a stale or failed reading does. Arming here would let a device that
        // slept on mains wake on its next good sample.
        let mut w = ResumeWatch::new(2, true);
        w.observe(true, false);
        w.break_run();
        assert!(!w.is_armed(), "a stale reading is evidence of nothing");
        assert_eq!(w.confirm_progress(), 0);
    }

    #[test]
    fn confirm_progress_is_reported_for_the_log() {
        let mut w = ResumeWatch::new(3, false);
        assert_eq!(w.confirm_progress(), 0);
        w.observe(true, false);
        assert_eq!(w.confirm_progress(), 1);
        w.observe(true, false);
        assert_eq!(w.confirm_progress(), 2);
    }

    #[test]
    fn wake_reasons_have_stable_audit_strings() {
        // These land in the encrypted audit log and in MQTT payloads.
        assert_eq!(WakeReason::DcPresent.as_str(), "dc_present");
        assert_eq!(WakeReason::Button.as_str(), "button");
    }

    #[test]
    fn a_wake_request_while_awake_is_ignored() {
        // Guards the awake path only; the flag is process-wide, so this asserts
        // nothing about a concurrent standby.
        clear_wake_request();
        request_wake(WakeReason::Button);
        assert!(
            take_wake_request().is_none(),
            "nothing to wake — the device is already up"
        );
    }
}
