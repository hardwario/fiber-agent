//! End-to-end test of the `fiberctl` binary against a live in-process control
//! server (#79). Spawns `control::server::serve` on a temp socket, then drives
//! the *actual compiled* `fiberctl` binary via `std::process::Command` and
//! asserts on its stdout/stderr/exit code. No hardware / no daemon needed
//! (ControlContext has `lorawan: None`).

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use fiber_app::libs::config::Config;
use fiber_app::libs::control::server::{serve, ControlContext};
use fiber_app::libs::leds::state::SharedLedStateWithNotify;
use fiber_app::libs::power::PowerStatus;
use fiber_app::libs::sensors::create_shared_sensor_state;
use std::sync::Mutex;

fn start_server() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock").to_string_lossy().to_string();
    let mut config = Config::default_config();
    if let Some(mqtt) = config.mqtt.as_mut() {
        mqtt.broker.password = Some("e2e-secret-password".to_string());
    }
    let ctx = ControlContext::new(
        "1.2.3-test".to_string(),
        Arc::new(config),
        None,
        None,
        Duration::from_millis(200),
    )
    .with_power(Arc::new(Mutex::new(PowerStatus::new(3700, 5000))))
    .with_sensors(create_shared_sensor_state())
    .with_led(Arc::new(SharedLedStateWithNotify::new()));
    let p = path.clone();
    std::thread::spawn(move || {
        let _ = serve(ctx, &p);
    });
    for _ in 0..200 {
        if std::path::Path::new(&path).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    (dir, path)
}

fn fiberctl(sock: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_fiberctl"))
        .args(["--socket", sock])
        .args(args)
        .output()
        .expect("run fiberctl")
}

#[test]
fn fiberctl_status() {
    let (_d, sock) = start_server();
    let out = fiberctl(&sock, &["status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("1.2.3-test"), "stdout: {stdout}");
    assert!(stdout.contains("lorawan"), "stdout: {stdout}");
}

#[test]
fn fiberctl_status_json_is_valid_json() {
    let (_d, sock) = start_server();
    let out = fiberctl(&sock, &["--json", "status"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert!(v["ok"].as_bool().unwrap());
    assert_eq!(v["data"]["app_version"], "1.2.3-test");
}

#[test]
fn fiberctl_config_get() {
    let (_d, sock) = start_server();
    let out = fiberctl(&sock, &["config", "get", "system.app_version"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("0.1.0"), "stdout: {stdout}"); // config default value
}

#[test]
fn fiberctl_config_show_does_not_leak_secret() {
    let (_d, sock) = start_server();
    let out = fiberctl(&sock, &["config", "show"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("e2e-secret-password"), "secret leaked to CLI output");
}

#[test]
fn fiberctl_config_get_missing_key_fails() {
    let (_d, sock) = start_server();
    let out = fiberctl(&sock, &["config", "get", "no.such.key"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no such config key"), "stderr: {stderr}");
}

#[test]
fn fiberctl_set_param_save_requires_force() {
    let (_d, sock) = start_server();
    let out = fiberctl(
        &sock,
        &["lorawan", "set-param", "5876070000000001", "application.interval_report=600", "--save"],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("force"), "stderr: {stderr}");
}

#[test]
fn fiberctl_send_without_device_reports_disabled() {
    let (_d, sock) = start_server();
    let out = fiberctl(&sock, &["lorawan", "send", "5876070000000001", "get-info"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("LoRaWAN is not enabled"), "stderr: {stderr}");
}

#[test]
fn fiberctl_bad_field_syntax_fails_client_side() {
    let (_d, sock) = start_server();
    // missing '=' → client-side parse error, exit code 2, no socket round-trip
    let out = fiberctl(&sock, &["lorawan", "set-param", "dev", "interval_report600"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("key=value"), "stderr: {stderr}");
}

#[test]
fn fiberctl_power_status() {
    let (_d, sock) = start_server();
    let out = fiberctl(&sock, &["--json", "power"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["vbat_mv"], 3700);
}

#[test]
fn fiberctl_sensors_read() {
    let (_d, sock) = start_server();
    let out = fiberctl(&sock, &["--json", "sensors", "read"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["sensors"].as_array().unwrap().len(), 8);
}

#[test]
fn fiberctl_led_set() {
    let (_d, sock) = start_server();
    let out = fiberctl(&sock, &["led", "set", "green", "--blink"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("green"));
}

#[test]
fn fiberctl_status_includes_power() {
    let (_d, sock) = start_server();
    let out = fiberctl(&sock, &["--json", "status"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["power"]["vbat_mv"], 3700);
}

#[test]
fn fiberctl_no_daemon_reports_connection_error() {
    // point at a non-existent socket
    let out = fiberctl("/tmp/fiber-nonexistent-xyz.sock", &["status"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot connect"), "stderr: {stderr}");
}
