//! `proximosctl --version` reports the build's version, the one `/health` and the BLE device
//! info report (`plan/system/design.md` §9.1): the release version on a release build,
//! the crate version on any other.

use std::process::Command;

#[test]
fn version_flag_prints_the_build_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_proximosctl"))
        .arg("--version")
        .output()
        .expect("proximosctl runs");
    assert!(out.status.success(), "exit {:?}", out.status);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("proximosctl {}", control_radio::VERSION)
    );
}
