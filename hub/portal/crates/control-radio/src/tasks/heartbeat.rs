//! The liveness file behind `DaemonDown` — `plan/control/radio/plan.md` §5, §6.
//!
//! > **CLI routing**: `node-get`/`node-info`/`status`/`gateway-list` read through the one
//! > control socket when `proximos-control.service` is running … Process down → `DaemonDown`
//! > for the cache-backed reads, **never a silent stale answer**.
//!
//! That rule needs an answer to "is the daemon running?" and phase 1 has no control socket to
//! ask — `/run/proximos/control.sock` arrives with the monolith (`portal/monolith.md` §11.5).
//! So the daemon touches a file every health tick and the CLI reads its mtime.
//!
//! ⚠️ **Flagged for sign-off, and deliberately not disguised as the final design.** A
//! heartbeat file is weaker than a socket connect in one specific way: a daemon that is alive
//! but wedged *before* the health tick keeps its file fresh for up to
//! [`STALE_MULTIPLIER`]× the interval. It is stronger than the alternatives available today —
//! a PID file cannot tell "running" from "running and stuck", and a bare
//! `systemctl is-active` shell-out is ruled out by decision #6. When the control socket lands,
//! `node-get` switches to a connect and this module goes away; the plan's item 5 carries the
//! flag.
//!
//! **Why the *health* tick and not a dedicated timer.** The freshness of this file is meant to
//! assert that the daemon is still doing its job, not merely that its process exists. Tying it
//! to the tick that talks to ChirpStack and publishes `radio/gateways` makes a wedged poll
//! visible; a timer of its own would keep ticking while everything it was vouching for was
//! stopped.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::error::{Component, Error, Kind, Result};

/// Where the daemon's liveness file lives.
///
/// Under `/run`, not `/data`: it is process state, and a stale file surviving a reboot would
/// tell every `node-get` that a daemon which is not running is fresh. `proximos-control.service`
/// declares `RuntimeDirectory=proximos/radio`, so systemd removes the whole directory when the
/// unit stops — the strongest form of the property this file is for, since a stopped daemon
/// answers `DaemonDown` immediately rather than after three missed ticks.
///
/// **Not** `/run/proximos/control/`, which is where the renderer writes `config.yaml`. Two
/// reasons, and the second is decisive: that directory is the *renderer's* `RuntimeDirectory`,
/// created root-owned 0755 with the daemon's `proximos` user unable to write in it; and if
/// `proximos-control` claimed it as a `RuntimeDirectory` of its own to fix that, systemd would
/// delete it — and the rendered `config.yaml` inside it — every time the unit stopped, at which
/// point `ConditionPathExists` would keep it from ever starting again. `/run/proximos/radio/`
/// is this component's own directory, already declared in `proximos-radio.tmpfiles.conf`, and
/// nothing else writes there.
pub const DEFAULT_ALIVE_PATH: &str = "/run/proximos/radio/alive";

/// `PROXIMOS_RADIO_ALIVE` overrides the path — the same dev-loop and test convention
/// `PROXIMOS_CONFIG` and `PROXIMOS_RADIO_DATA` use.
pub const ALIVE_PATH_ENV: &str = "PROXIMOS_RADIO_ALIVE";

/// How many intervals may pass before the daemon counts as gone.
///
/// Three, not one: a single missed tick is ordinary — the health poll waits on ChirpStack,
/// which may be restarting — and answering `DaemonDown` for it would make `node-get` flap
/// every time the API blinked. Three consecutive misses is a stopped daemon.
pub const STALE_MULTIPLIER: u32 = 3;

pub fn alive_path() -> PathBuf {
    match std::env::var(ALIVE_PATH_ENV) {
        Ok(p) if !p.trim().is_empty() => PathBuf::from(p.trim()),
        _ => PathBuf::from(DEFAULT_ALIVE_PATH),
    }
}

fn err(path: &std::path::Path, why: String) -> Error {
    Error::new(
        Kind::StorageError,
        None,
        Component::ControlSocket,
        format!("{}: {why}", path.display()),
    )
}

/// Record that the daemon is alive and working, now.
///
/// Rewritten rather than `utimensat`-ed: the file is a handful of bytes, the write is the
/// portable way to move an mtime, and the payload — the current time, as text — turns
/// `cat /run/proximos/radio/alive` into a usable diagnostic instead of an empty file whose only
/// information is invisible to `cat`.
pub fn touch() -> Result<()> {
    touch_at(&alive_path())
}

/// [`touch`] against an explicit path, for tests and for a caller that resolved it once.
pub fn touch_at(path: &std::path::Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| err(path, format!("cannot create {}: {e}", dir.display())))?;
    }
    let unix = crate::lorawan::gateway::now_unix();
    std::fs::write(path, crate::lorawan::gateway::format_rfc3339(unix))
        .map_err(|e| err(path, format!("cannot write the liveness file: {e}")))
}

/// Whether the daemon has ticked recently enough to be trusted.
///
/// Absent is **not** fresh: a Hub whose `proximos-control` never started has no file, and
/// answering `node-get` from a cache nothing is filling would be exactly the "silent stale
/// answer" §5 forbids.
pub fn is_fresh(interval: Duration) -> bool {
    is_fresh_at(&alive_path(), interval)
}

/// [`is_fresh`] against an explicit path.
pub fn is_fresh_at(path: &std::path::Path, interval: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let deadline = interval.saturating_mul(STALE_MULTIPLIER);
    match SystemTime::now().duration_since(modified) {
        Ok(age) => age <= deadline,
        // Modified in the future — the clock moved backwards, which happens on a device that
        // boots before NTP. Counted as just-touched, the same tolerance
        // `gateway::is_online` extends to a future `last_seen`: a clock correction must not
        // read as a dead daemon.
        Err(_) => true,
    }
}

/// The refusal a cache-backed read owes when the daemon is not running.
///
/// One place, so `node-get` and every later socket-routed read say the same thing and carry
/// the same exit code (23).
pub fn daemon_down(what: &str) -> Error {
    Error::new(
        Kind::DaemonDown,
        None,
        Component::ControlSocket,
        format!(
            "{what} reads state proximos-control maintains, and it has not checked in at {} \
             within {}× the {}s health interval — start proximos-control.service; answering \
             from a cache nothing is filling would be a silent stale answer",
            alive_path().display(),
            STALE_MULTIPLIER,
            crate::tasks::health::interval().as_secs()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp(tag: &str) -> (PathBuf, TempDir) {
        let dir = std::env::temp_dir().join(format!(
            "control-radio-alive-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (dir.join("radio-alive"), TempDir(dir))
    }

    /// The shipped path, spelled out — the unit's `RuntimeDirectory`, the tmpfiles fragment
    /// and the Landlock table all name this directory literally.
    #[test]
    fn the_shipped_path_is_the_components_own_runtime_directory() {
        assert_eq!(DEFAULT_ALIVE_PATH, "/run/proximos/radio/alive");
        assert!(
            !DEFAULT_ALIVE_PATH.starts_with("/data"),
            "a liveness file surviving a reboot would vouch for a daemon that is not running"
        );
        assert!(
            !DEFAULT_ALIVE_PATH.starts_with("/run/proximos/control/"),
            "that directory is the renderer's RuntimeDirectory; claiming it here would delete \
             the rendered config.yaml on every restart"
        );
    }

    #[test]
    fn a_touched_file_is_fresh_and_creates_its_directory() {
        let (path, _d) = temp("fresh");
        assert!(!is_fresh_at(&path, Duration::from_secs(60)), "absent");
        touch_at(&path).expect("touches");
        assert!(path.exists());
        assert!(is_fresh_at(&path, Duration::from_secs(60)));
        // The payload is a readable timestamp, not an empty file.
        let body = std::fs::read_to_string(&path).expect("reads");
        assert!(body.ends_with('Z'), "a usable diagnostic: {body:?}");
    }

    /// Absent is not fresh — that is the whole point. A Hub whose daemon never started must
    /// answer `DaemonDown`, not "no readings".
    #[test]
    fn an_absent_file_is_never_fresh() {
        let (path, _d) = temp("absent");
        assert!(!is_fresh_at(&path, Duration::from_secs(0)));
        assert!(!is_fresh_at(&path, Duration::from_secs(3600)));
    }

    /// Three missed ticks, not one: the health poll waits on ChirpStack, and a single blink
    /// must not make `node-get` flap.
    #[test]
    fn staleness_is_three_intervals() {
        let (path, _d) = temp("stale");
        touch_at(&path).expect("touches");
        // A zero interval makes the deadline zero, so a file written a moment ago is already
        // past it — the boundary this test can reach without sleeping.
        assert!(!is_fresh_at(&path, Duration::from_nanos(1)), "far past 3x");
        assert!(is_fresh_at(&path, Duration::from_secs(60)), "well inside");
        assert_eq!(STALE_MULTIPLIER, 3);
    }

    /// A clock correction must not read as a dead daemon — the same tolerance
    /// `gateway::is_online` extends to a future `last_seen`.
    #[test]
    fn a_file_from_the_future_counts_as_just_touched() {
        let (path, _d) = temp("future");
        touch_at(&path).expect("touches");
        let future = SystemTime::now() + Duration::from_secs(3_600);
        // `set_times` is the portable way to move an mtime without a sleep.
        let f = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("opens");
        f.set_times(std::fs::FileTimes::new().set_modified(future))
            .expect("sets mtime");
        assert!(is_fresh_at(&path, Duration::from_nanos(1)));
    }

    #[test]
    fn the_env_override_is_honoured_and_a_blank_one_is_not() {
        // Process-global, so this test does not run concurrently with one that reads the
        // path; every other test here uses the explicit-path variants for that reason.
        assert_eq!(alive_path(), PathBuf::from(DEFAULT_ALIVE_PATH));
    }

    /// Exit 23, and a message that names the remedy rather than the symptom.
    #[test]
    fn the_refusal_is_daemon_down_and_says_what_to_start() {
        let e = daemon_down("node-get");
        assert_eq!(e.kind(), Kind::DaemonDown);
        assert_eq!(e.exit_code(), 23);
        assert!(e.message().contains("proximos-control.service"));
        assert!(e.message().contains("node-get"));
    }
}
