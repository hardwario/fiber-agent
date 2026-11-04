//! The daemon as the unit file actually runs it.
//!
//! These drive the built binary rather than a library entry point, because the two behaviours
//! they pin — *stay alive under `radio.mode: off`* and *exit 0 on SIGTERM* — are statements
//! about a **process**, and a test that called an `async fn run()` would prove neither. The
//! second one is what `Restart=on-failure` keys on: a clean stop must not look like a crash.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_proximos-control");

struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp(tag: &str) -> (PathBuf, TempDir) {
    let dir = std::env::temp_dir().join(format!(
        "proximos-control-{}-{tag}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    (dir.clone(), TempDir(dir))
}

/// A device config with the radio switched off. `"off"` is quoted deliberately: YAML 1.1 reads
/// a bare `off` as the boolean `false`, and the enum wants the string.
fn off_config(dir: &Path) -> PathBuf {
    let path = dir.join("config.yaml");
    let mut f = std::fs::File::create(&path).expect("config");
    f.write_all(
        br#"radio:
  region: eu868
  mode: "off"
  application:
    name: "ProXimos Sensors"
  device_profiles: {}
  gateway:
    name: "ProXimos Gateway"
"#,
    )
    .expect("write");
    path
}

fn spawn(config: &Path, data: &Path) -> Child {
    Command::new(BIN)
        .env("PROXIMOS_CONFIG", config)
        .env("PROXIMOS_RADIO_DATA", data)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawns")
}

fn sigterm(child: &Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("kill");
    assert!(status.success(), "could not signal the daemon");
}

/// Wait for the process to exit, up to a deadline.
fn wait_for_exit(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        match child.try_wait().expect("try_wait") {
            Some(status) => return Some(status),
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    None
}

/// `radio.mode: off` means there is no radio to serve — it does **not** mean this unit has
/// nothing to do. `portal/monolith.md` §9 makes this process the agent's host, so it stays up;
/// a daemon that exited here would leave `systemctl status` reading `inactive (dead)` on a Hub
/// whose operator has no reason to think anything is wrong, and would have to be re-enabled by
/// hand the day the first non-radio subsystem lands in it.
#[test]
fn radio_mode_off_stays_alive_and_stops_cleanly() {
    let (dir, _guard) = temp("off");
    let config = off_config(&dir);
    let mut child = spawn(&config, &dir);

    // Still running after a moment — not a fast clean exit.
    assert!(
        wait_for_exit(&mut child, Duration::from_millis(500)).is_none(),
        "the daemon exited instead of idling"
    );

    sigterm(&child);
    let status = wait_for_exit(&mut child, Duration::from_secs(10)).unwrap_or_else(|| {
        let _ = child.kill();
        panic!("SIGTERM did not stop the daemon within 10 s");
    });
    assert!(
        status.success(),
        "a clean stop must not look like a crash to Restart=on-failure: {status:?}"
    );
}

/// `ConditionPathExists=/run/proximos/control/config.yaml` keeps the unit out of the service
/// set when the renderer produced nothing. If it runs anyway, the exit code has to be the §6
/// taxonomy's `NotBootstrapped` (10) and not an ad-hoc number — that is what an operator reads
/// out of `systemctl status`.
#[test]
fn a_missing_config_exits_with_the_taxonomys_code() {
    let (dir, _guard) = temp("noconfig");
    let mut child = spawn(&dir.join("does-not-exist.yaml"), &dir);
    let status = wait_for_exit(&mut child, Duration::from_secs(10)).unwrap_or_else(|| {
        let _ = child.kill();
        panic!("the daemon hung on a missing config");
    });
    assert_eq!(
        status.code(),
        Some(10),
        "NotBootstrapped is exit 10 (plan.md §6)"
    );
}
