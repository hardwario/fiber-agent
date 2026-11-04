//! `proximosctl control.radio create --radio lorawan --role client`, the CLI twin of BLE FB1C
//! (plan/control/cluster.md §8): the real binary against a stand-in for the agent's control
//! socket. What is pinned here is what only the binary can show — the password arrives on
//! stdin and reaches the socket, and never argv, stdout or stderr — plus the exit codes from the
//! shared table.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const PASSWORD: &str = "Zm9vYmFyYmF6cXV4cXV1eA";
const SERVER: &str = "proximos-1a2b3c4d.local";
const FINGERPRINT: &str = "4Fg1yj5x8x2R0T3y4bWcL9e6t5Qa8n2rJ7kPq0sVwXz";

/// The device config, its `radio.mode` set to `mode`.
fn device_config(dir: &Path, mode: &str) -> PathBuf {
    let path = dir.join("fiber.config.yaml");
    let fixture = include_str!("../../control-radio/tests/fixtures/radio-section.yaml");
    let config = fixture.replacen("  mode: lorawan ", &format!("  mode: {mode} "), 1);
    assert!(
        config.contains(&format!("  mode: {mode} ")),
        "the fixture's mode line moved"
    );
    std::fs::write(&path, config).unwrap();
    path
}

/// An agent that answers one request with `answer`, and hands back the line it received.
fn agent(path: &Path, answer: &'static str) -> std::thread::JoinHandle<String> {
    let listener = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        (&stream).write_all(answer.as_bytes()).unwrap();
        (&stream).write_all(b"\n").unwrap();
        line
    })
}

fn join(dir: &Path, socket: &Path, stdin: &str, extra: &[&str]) -> (Output, Vec<String>) {
    join_in_mode(dir, "lorawan", socket, stdin, extra)
}

/// [`join`] on a Hub whose `radio.mode` is `mode`.
fn join_in_mode(
    dir: &Path,
    mode: &str,
    socket: &Path,
    stdin: &str,
    extra: &[&str],
) -> (Output, Vec<String>) {
    let mut args: Vec<String> = [
        "control.radio",
        "create",
        "--radio",
        "lorawan",
        "--role",
        "client",
        "--server-host",
        SERVER,
        "--ca-sha256",
        FINGERPRINT,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(extra.iter().map(|s| s.to_string()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_proximosctl"))
        .args(&args)
        .env("PROXIMOS_CONFIG", device_config(dir, mode))
        .env("PROXIMOS_RADIO_DATA", dir.join("radio"))
        .env("FIBER_CONTROL_SOCKET", socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("running proximosctl");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    (child.wait_with_output().unwrap(), args)
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn the_password_comes_from_stdin_and_goes_only_to_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("control.sock");
    let agent = agent(
        &socket,
        r#"{"ok":true,"data":{"server_host":"proximos-1a2b3c4d.local","message":"joined proximos-1a2b3c4d.local; restarting","reboot_in_s":5}}"#,
    );
    let (out, argv) = join(dir.path(), &socket, &format!("{PASSWORD}\n"), &["--json"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(argv.iter().all(|a| !a.contains(PASSWORD)), "not on argv");
    assert!(!text(&out).contains(PASSWORD), "not echoed: {}", text(&out));

    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["server_host"], SERVER);
    assert_eq!(doc["role"], "client");
    assert_eq!(doc["reboot_in_s"], 5);

    let sent: serde_json::Value = serde_json::from_str(agent.join().unwrap().trim_end()).unwrap();
    assert_eq!(
        sent,
        serde_json::json!({"v": 1, "cmd": {
            "type": "cluster_join",
            "server_host": SERVER,
            "password": PASSWORD,
            "ca_sha256": FINGERPRINT,
        }})
    );
}

#[test]
fn a_refusal_exits_with_the_shared_table_and_never_shows_the_password() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("control.sock");
    let agent = agent(
        &socket,
        r#"{"ok":false,"error":"the broker refused peer-proximos-5e6f7a8b: not authorized","error_code":"broker_refused"}"#,
    );
    let (out, _) = join(dir.path(), &socket, &format!("{PASSWORD}\n"), &[]);
    assert_eq!(
        out.status.code(),
        Some(11),
        "Unauthenticated: {}",
        text(&out)
    );
    assert!(text(&out).contains("not authorized"), "{}", text(&out));
    assert!(!text(&out).contains(PASSWORD), "{}", text(&out));
    agent.join().unwrap();
}

#[test]
fn an_empty_stdin_is_refused_before_the_agent_is_asked() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("nobody.sock");
    let (out, _) = join(dir.path(), &socket, "", &[]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("stdin"), "{}", text(&out));

    // With a password but no agent: DaemonDown.
    let (out, _) = join(dir.path(), &socket, &format!("{PASSWORD}\n"), &[]);
    assert_eq!(out.status.code(), Some(23), "{}", text(&out));
    assert!(!text(&out).contains(PASSWORD));
}

/// Decision #47: the join no longer gates on this Hub's radio mode. Every mode reaches the
/// agent; with none listening that is DaemonDown (23).
#[test]
fn every_radio_mode_reaches_the_agent() {
    for mode in ["off", "p2p"] {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("nobody.sock");
        let (out, _) = join_in_mode(dir.path(), mode, &socket, &format!("{PASSWORD}\n"), &[]);
        assert_eq!(out.status.code(), Some(23), "{mode}: {}", text(&out));
        assert!(!text(&out).contains(PASSWORD), "{mode}: {}", text(&out));
    }
}

/// `lorawan` and `dual` both have LoRaWAN: the join reaches the agent.
#[test]
fn a_hub_on_lorawan_or_dual_reaches_the_agent() {
    for mode in ["lorawan", "dual"] {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("control.sock");
        let agent = agent(
            &socket,
            r#"{"ok":true,"data":{"server_host":"proximos-1a2b3c4d.local","message":"joined proximos-1a2b3c4d.local; restarting","reboot_in_s":5}}"#,
        );
        let (out, _) = join_in_mode(dir.path(), mode, &socket, &format!("{PASSWORD}\n"), &[]);
        assert!(out.status.success(), "{mode}: {}", text(&out));
        let sent = agent.join().unwrap();
        assert!(sent.contains("cluster_join"), "{mode}: {sent}");
    }
}

/// The agent's own radio check (the apply path FB1C shares) answers `not_joinable`, for a Hub
/// whose chosen mode differs from what the CLI read: exit 14, the mode named.
#[test]
fn the_agents_radio_mode_refusal_exits_14() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("control.sock");
    let agent = agent(
        &socket,
        r#"{"ok":false,"error":"radio mode is p2p; set LoRaWAN (FB1B) before joining","error_code":"not_joinable"}"#,
    );
    let (out, _) = join(dir.path(), &socket, &format!("{PASSWORD}\n"), &[]);
    assert_eq!(out.status.code(), Some(14), "AlreadyExists: {}", text(&out));
    assert!(
        text(&out).contains("radio mode is p2p; set LoRaWAN (FB1B) before joining"),
        "{}",
        text(&out)
    );
    agent.join().unwrap();
}
