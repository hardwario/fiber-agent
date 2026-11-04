//! `proximosctl render`, driven the way `proximos-render.service` drives it: the real
//! binary, no arguments, everything through the environment.
//!
//! The unit's `ExecStart=` is `/usr/bin/proximosctl render` and nothing more, so this is
//! the whole of its interface. The four `PROXIMOS_*` variables point the run at a temp
//! directory instead of `/data` and `/run` — the same seam the library's own tests use,
//! exercised here through argv and env rather than through a Rust struct.

use std::path::{Path, PathBuf};
use std::process::Command;

fn templates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/templates")
}

struct Run {
    dir: tempfile::TempDir,
    data: PathBuf,
    run: PathBuf,
}

impl Run {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let run = dir.path().join("run/proximos");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        // What migration 005-chirpstack-seed.sh leaves behind, so the LoRaWAN half is
        // renderable.
        std::fs::create_dir_all(data.join("chirpstack")).unwrap();
        std::fs::write(
            data.join("chirpstack/api-secret"),
            "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
        )
        .unwrap();
        Self { dir, data, run }
    }

    fn overlay(self, yaml: &str) -> Self {
        std::fs::create_dir_all(self.data.join("proximos/config")).unwrap();
        std::fs::write(self.data.join("proximos/config/proximos.config.yaml"), yaml).unwrap();
        self
    }

    fn exec(&self, extra: &[(&str, &Path)]) -> std::process::Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_proximosctl"));
        cmd.arg("render")
            .env("PROXIMOS_TEMPLATES", templates_dir())
            .env("PROXIMOS_DATA_ROOT", &self.data)
            .env("PROXIMOS_RUN_ROOT", &self.run);
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.output().expect("running proximosctl render")
    }
}

fn stdout(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn render_writes_every_config_and_reports_each_one() {
    // The overlay names the mode, because without one the binary asks the machine it runs on
    // for the RAK5146 (node_registry.md §12) — and a test runner has no card.
    let r = Run::new().overlay("radio:\n  mode: lorawan\n");
    let out = r.exec(&[]);

    assert!(out.status.success(), "{}", stderr(&out));
    let lines = stdout(&out);
    for expected in [
        "portal/config.yaml",
        "control/config.yaml",
        "chirpstack/chirpstack.toml",
        "chirpstack/region_eu868.toml",
        "chirpstack-mqtt-forwarder/chirpstack-mqtt-forwarder.toml",
    ] {
        assert!(
            lines.contains(expected),
            "{expected} not reported:\n{lines}"
        );
        assert!(r.run.join(expected).exists(), "{expected} not written");
    }
}

/// With no mode named anywhere, the render still succeeds: the binary probes for the card and
/// says which default it took, one of the two the hardware table allows (§12).
#[test]
fn with_no_mode_named_the_binary_reports_the_hardware_default() {
    let r = Run::new();
    let out = r.exec(&[]);

    assert!(out.status.success(), "{}", stderr(&out));
    let lines = stdout(&out);
    let default = if lines.contains("hardware default radio.mode lorawan") {
        "lorawan"
    } else {
        assert!(
            lines.contains("hardware default radio.mode p2p"),
            "the default is lorawan or p2p:\n{lines}"
        );
        "p2p"
    };
    let control = std::fs::read_to_string(r.run.join("control/config.yaml")).unwrap();
    assert!(control.contains(&format!("mode: {default}")), "{control}");
}

/// A skip is not a failure: exit 0, with the reason on stderr where a
/// `journalctl -u proximos-render` reader will find it next to the condition-skipped unit
/// it explains.
#[test]
fn a_gated_mode_still_exits_zero_and_says_why() {
    let r = Run::new().overlay("radio:\n  mode: p2p\n");
    let out = r.exec(&[]);

    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("chirpstack.toml"), "{err}");
    assert!(err.contains("radio.mode is p2p"), "{err}");
    assert!(!r.run.join("chirpstack/chirpstack.toml").exists());
    assert!(r.run.join("portal/config.yaml").exists());
}

/// A broken overlay is exit 1 with the reason on stderr. Systemd cares only about
/// zero/non-zero, which is why this is a plain 1 and not a code borrowed from
/// control-radio's exit-code taxonomy — that table means something specific to
/// `control.radio` callers.
#[test]
fn a_broken_overlay_fails_with_exit_one_and_a_readable_message() {
    let r = Run::new().overlay("radio:\n  mode: sometimes\n");
    let out = r.exec(&[]);

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.starts_with("proximosctl render:"), "{err}");
    assert!(err.contains("sometimes"), "{err}");
    assert!(!r.run.join("portal/config.yaml").exists());
}

/// `PROXIMOS_CONFIG` is control-radio's variable for the file it *reads*. The renderer
/// writes the same one, so the dev loop and the integration stack can point both ends at
/// one path.
#[test]
fn proximos_config_moves_the_control_config() {
    let r = Run::new();
    let elsewhere = r.dir.path().join("elsewhere/config.yaml");
    let out = r.exec(&[("PROXIMOS_CONFIG", &elsewhere)]);

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(elsewhere.exists());
    assert!(!r.run.join("control/config.yaml").exists());
}

/// Ownership is reported, never silently assumed. The test suite is not root, so this is
/// the branch that always fires here — and it is the one that must not be mistaken for a
/// successful chown when reading a journal.
#[test]
fn a_non_root_run_says_that_ownership_was_not_applied() {
    let r = Run::new();
    let out = r.exec(&[]);
    assert!(out.status.success());
    let err = stderr(&out);
    // Skipped only when this test genuinely runs unprivileged, which is the normal case;
    // a root CI runner legitimately gets the other branch.
    if !nix_is_root() {
        assert!(err.contains("ownership was not"), "{err}");
    }
}

fn nix_is_root() -> bool {
    // No libc dependency for one number.
    std::fs::metadata("/proc/self")
        .ok()
        .map(|_| std::env::var("USER").map(|u| u == "root").unwrap_or(false))
        .unwrap_or(false)
}

/// The two groups are siblings in one CLI, and `render` takes no arguments.
#[test]
fn the_cli_tree_carries_both_groups() {
    let out = Command::new(env!("CARGO_BIN_EXE_proximosctl"))
        .arg("--help")
        .output()
        .unwrap();
    let text = stdout(&out);
    assert!(text.contains("control.radio"), "{text}");
    assert!(text.contains("render"), "{text}");
}
