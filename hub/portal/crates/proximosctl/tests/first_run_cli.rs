//! `proximosctl first-run-apply`, driven the way `proximos-first-run.service` drives it:
//! the real binary, no arguments, everything through the environment. The exit code is
//! the unit's whole interface, since the unit reboots on 3 and on nothing else.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn templates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/templates")
}

struct Roots {
    _dir: tempfile::TempDir,
    data: PathBuf,
    run: PathBuf,
}

impl Roots {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let run = dir.path().join("run/proximos");
        std::fs::create_dir_all(data.join("portal")).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        Self {
            _dir: dir,
            data,
            run,
        }
    }

    fn exec(&self, verb: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_proximosctl"))
            .arg(verb)
            .env("PROXIMOS_TEMPLATES", templates_dir())
            .env("PROXIMOS_DATA_ROOT", &self.data)
            .env("PROXIMOS_RUN_ROOT", &self.run)
            .env_remove("PROXIMOS_CONFIG")
            .output()
            .expect("running proximosctl")
    }

    fn request(&self, mode: &str) {
        std::fs::write(self.data.join("portal/requested-radio-mode"), mode).unwrap();
    }

    fn rendered_control(&self) -> String {
        std::fs::read_to_string(self.run.join("control/config.yaml")).unwrap()
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn no_request_exits_zero() {
    let r = Roots::new();
    let out = r.exec("first-run-apply");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("no request"));
}

#[test]
fn a_changed_mode_exits_three_and_the_next_render_uses_it() {
    let r = Roots::new();
    assert!(r.exec("render").status.success());
    // The first render runs the hardware default (node_registry.md §12): lorawan with the
    // RAK5146, p2p without it. Ask for the other one, whichever machine this runs on.
    let other = if r.rendered_control().contains("mode: lorawan") {
        "p2p"
    } else {
        assert!(r.rendered_control().contains("mode: p2p"));
        "lorawan"
    };

    r.request(&format!("{other}\n"));
    let out = r.exec("first-run-apply");
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(!r.data.join("portal/requested-radio-mode").exists());

    // The "reboot": the next boot's render.
    assert!(r.exec("render").status.success());
    assert!(r.rendered_control().contains(&format!("mode: {other}")));
}

#[test]
fn a_request_before_the_render_exits_zero() {
    let r = Roots::new();
    r.request("p2p");
    let out = r.exec("first-run-apply");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(r.exec("render").status.success());
    assert!(r.rendered_control().contains("mode: p2p"));
}

#[test]
fn a_refused_request_exits_zero_and_says_why() {
    let r = Roots::new();
    r.request("p2p");
    r.exec("first-run-apply");
    r.request("lorawan");
    let out = r.exec("first-run-apply");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("refused"), "{}", text(&out));
}

#[test]
fn the_unit_reboots_on_exactly_the_exit_code_the_cli_uses_for_it() {
    let unit = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/proximos-first-run.service"),
    )
    .unwrap();
    let exec = unit
        .lines()
        .find(|l| l.starts_with("ExecStart="))
        .expect("ExecStart=");
    assert!(
        exec.contains("/usr/bin/proximosctl first-run-apply"),
        "{exec}"
    );
    assert!(exec.contains(r#"[ "$rc" -eq 3 ]"#), "{exec}");
    assert!(exec.contains("systemctl --no-block reboot"), "{exec}");
}
