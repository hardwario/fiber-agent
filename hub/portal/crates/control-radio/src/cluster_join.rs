//! `create --radio lorawan|p2p --role client` — join this Hub to a Server Hub as a **Client**
//! (`plan/control/cluster.md` §8, the CLI twin of BLE FB1C; `lorawan.md` §8.2;
//! `p2p_cluster_gateway.md` §3.1).
//!
//! The join is not done here. It runs in the agent (`fiber_app`), the one root process, through
//! the **same apply path FB1C takes** (control-core's `libs::cluster_join::run`). That path
//! checks that the Hub is not set up, has no role marker and runs a radio (`lorawan`, `p2p` or
//! `dual`; this crate's mode gate refuses the named arm first), validates the bundle, writes the
//! Server's hosts line, fetches the Server's CA and checks it against `--ca-sha256`, logs in to
//! the broker, writes `/data/proximos/cluster/*` (the connection files, then `role`) and then
//! completes setup as a Client with a `CLUSTER_JOIN` audit row (`origin: cli`), and then
//! **reboots** the Hub (cluster.md §8, amended 2026-10-02). This module is a client of the
//! agent's control socket (`/run/fiber/control.sock`, root only, so the filesystem permission
//! is the authorization, decision #10). It speaks the socket's one-line JSON protocol, the way
//! `proximosctl cluster` does.
//!
//! Decision #12 keeps this crate free of workspace dependencies, so the request is written out
//! here. Both sides pin the same string: [`tests::the_request_is_the_one_the_agent_parses`]
//! here, and control-core's `the_cluster_join_request_proximosctl_sends_parses`.
//!
//! **The password never reaches argv** (ledger #22, `tests/clap_tree.rs`). It is the first line
//! of stdin. It goes into the request line and nowhere else: no log line, no error and no result
//! here carries it, and the agent keeps it only in the 0600 `peer_password` file.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{Component, Error, Kind, Result, Transport};

/// The agent's control socket, as control-core's `protocol::DEFAULT_SOCKET_PATH` names it.
pub const DEFAULT_SOCKET: &str = "/run/fiber/control.sock";
/// The same override control-core reads, so a dev agent on another path is reachable.
pub const SOCKET_ENV: &str = "FIBER_CONTROL_SOCKET";
/// control-core's `protocol::PROTOCOL_VERSION`.
const PROTOCOL_VERSION: u32 = 1;
/// The join is bounded at 20 s once the name is resolved, and avahi gives up after about 10 s.
/// This leaves room for both, plus the agent's own setup write.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);
/// A password line longer than this is not a 22-character password. Reading stops here, so a
/// stray file piped in by mistake cannot make the CLI allocate without limit.
const MAX_PASSWORD_LINE: u64 = 256;

pub fn socket_path() -> String {
    std::env::var(SOCKET_ENV).unwrap_or_else(|_| DEFAULT_SOCKET.to_string())
}

fn usage(message: impl Into<String>) -> Error {
    Error::new(
        Kind::InvalidArgument,
        Transport::Lorawan,
        Component::ControlSocket,
        message,
    )
}

/// The broker password: the first line of `input`, without its line ending. Anything after the
/// first line is ignored. The value is not judged here; the agent's validation is the one that
/// counts, so the CLI and FB1C refuse the same bundles.
pub fn read_password(input: &mut dyn BufRead) -> Result<String> {
    let mut line = String::new();
    std::io::Read::take(&mut *input, MAX_PASSWORD_LINE)
        .read_line(&mut line)
        .map_err(|e| {
            usage(format!(
                "cannot read the Server's broker password from stdin: {e}"
            ))
        })?;
    let password = line.trim_end_matches(['\n', '\r']);
    if password.is_empty() {
        return Err(usage(
            "--role client reads the Server's broker password (the join bundle's p) from \
             stdin, and stdin was empty",
        ));
    }
    Ok(password.to_string())
}

/// The one request line.
pub fn request(server_host: &str, password: &str, ca_sha256: &str) -> Value {
    json!({
        "v": PROTOCOL_VERSION,
        "cmd": {
            "type": "cluster_join",
            "server_host": server_host,
            "password": password,
            "ca_sha256": ca_sha256,
        }
    })
}

/// One request, one answer line, then the connection closes. A missing agent is `DaemonDown`.
pub fn send(path: &str, request: &Value) -> Result<Value> {
    let daemon = |m: String| {
        Error::new(
            Kind::DaemonDown,
            Transport::Lorawan,
            Component::ControlSocket,
            m,
        )
    };
    let stream = UnixStream::connect(path).map_err(|e| {
        daemon(format!(
            "cannot reach the agent at {path}: {e} (is fiber.service running, and are you root?)"
        ))
    })?;
    let _ = stream.set_read_timeout(Some(ANSWER_TIMEOUT));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let mut line = request.to_string();
    line.push('\n');
    (&stream)
        .write_all(line.as_bytes())
        .and_then(|()| (&stream).flush())
        .map_err(|e| daemon(format!("cannot send the join to the agent: {e}")))?;
    let mut answer = String::new();
    let n = BufReader::new(&stream)
        .read_line(&mut answer)
        .map_err(|e| daemon(format!("no answer from the agent: {e}")))?;
    if n == 0 {
        return Err(daemon(
            "the agent closed the connection without answering".to_string(),
        ));
    }
    serde_json::from_str(answer.trim_end()).map_err(|e| {
        Error::new(
            Kind::Transport,
            Transport::Lorawan,
            Component::ControlSocket,
            format!("cannot decode the agent's answer: {e}"),
        )
    })
}

/// The join's refusal codes (control-core's `cluster_join::FailKind`, and the socket's own),
/// onto this crate's exit-code table. The table only grows, so no number is added; each refusal
/// takes the existing meaning closest to it.
fn kind_of(error_code: Option<&str>) -> (Kind, Component) {
    match error_code {
        // Setup is complete, the Hub already has a role marker, or its radio is off (the
        // agent's own check, when its mode is not the one this CLI read).
        Some("not_joinable") => (Kind::AlreadyExists, Component::ControlSocket),
        // The bundle (or this Hub's own hostname) does not validate, or the Server's CA does
        // not match --ca-sha256: either way the arguments are what the operator must change.
        Some("invalid") | Some("ca_mismatch") => (Kind::InvalidArgument, Component::ControlSocket),
        // The broker refused the account: the credential exists, the far side refuses it.
        Some("broker_refused") => (Kind::Unauthenticated, Component::MqttBroker),
        // The Server does not resolve to IPv4 or does not answer in time: retryable.
        Some("unreachable") => (Kind::ServiceDown, Component::MqttBroker),
        Some("storage") => (Kind::StorageError, Component::ControlSocket),
        // The agent runs without its config applier.
        Some("not_enabled") => (Kind::DaemonDown, Component::ControlSocket),
        // Another join is running, or anything this build does not know.
        _ => (Kind::Transport, Component::ControlSocket),
    }
}

/// The agent's answer, as this command's result or error.
pub fn outcome(answer: &Value) -> Result<Value> {
    if answer.get("ok").and_then(Value::as_bool) == Some(true) {
        let mut data = answer.get("data").cloned().unwrap_or(Value::Null);
        if let Some(d) = data.as_object_mut() {
            d.insert("role".into(), json!("client"));
        }
        return Ok(data);
    }
    let message = answer
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("the agent refused the join without saying why");
    let (kind, component) = kind_of(answer.get("error_code").and_then(Value::as_str));
    Err(Error::new(
        kind,
        Transport::Lorawan,
        component,
        format!("the cluster join was refused: {message}"),
    ))
}

/// `create --role client` on the agent at `socket`, the password read from `stdin`.
pub fn run(
    server_host: &str,
    ca_sha256: &str,
    stdin: &mut dyn BufRead,
    socket: &str,
) -> Result<Value> {
    let password = read_password(stdin)?;
    let answer = send(socket, &request(server_host, &password, ca_sha256))?;
    outcome(&answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    const PASSWORD: &str = "Zm9vYmFyYmF6cXV4cXV1eA";

    /// The string control-core's `the_cluster_join_request_proximosctl_sends_parses` parses.
    #[test]
    fn the_request_is_the_one_the_agent_parses() {
        assert_eq!(
            request("proximos-1a2b3c4d.local", PASSWORD, "x"),
            serde_json::from_str::<Value>(
                r#"{"v":1,"cmd":{"type":"cluster_join","server_host":"proximos-1a2b3c4d.local","password":"Zm9vYmFyYmF6cXV4cXV1eA","ca_sha256":"x"}}"#
            )
            .unwrap()
        );
    }

    #[test]
    fn the_password_is_the_first_line_of_stdin() {
        for input in [
            format!("{PASSWORD}\n"),
            format!("{PASSWORD}\r\n"),
            PASSWORD.to_string(),
            format!("{PASSWORD}\nsomething else\n"),
        ] {
            let got = read_password(&mut input.as_bytes()).unwrap();
            assert_eq!(got, PASSWORD, "{input:?}");
        }
        for empty in ["", "\n", "\r\n"] {
            let e = read_password(&mut empty.as_bytes()).unwrap_err();
            assert_eq!(e.kind(), Kind::InvalidArgument);
            assert!(e.message().contains("stdin"), "{}", e.message());
        }
        // A long line is cut at the bound rather than read to the end.
        let long = "a".repeat(10_000);
        assert_eq!(
            read_password(&mut long.as_bytes()).unwrap().len(),
            MAX_PASSWORD_LINE as usize
        );
    }

    #[test]
    fn answers_map_onto_the_exit_code_table() {
        let ok = outcome(&json!({"ok": true, "data": {
            "server_host": "proximos-1a2b3c4d.local",
            "message": "joined proximos-1a2b3c4d.local; restarting",
            "reboot_in_s": 5
        }}))
        .unwrap();
        assert_eq!(ok["role"], "client");
        assert_eq!(ok["reboot_in_s"], 5);

        for (code, kind) in [
            ("not_joinable", Kind::AlreadyExists),
            ("invalid", Kind::InvalidArgument),
            ("ca_mismatch", Kind::InvalidArgument),
            ("broker_refused", Kind::Unauthenticated),
            ("unreachable", Kind::ServiceDown),
            ("storage", Kind::StorageError),
            ("not_enabled", Kind::DaemonDown),
            ("busy", Kind::Transport),
        ] {
            let e = outcome(&json!({"ok": false, "error": "why", "error_code": code})).unwrap_err();
            assert_eq!(e.kind(), kind, "{code}");
            assert!(e.message().contains("why"), "{code}: {}", e.message());
        }
        assert!(Kind::ServiceDown.is_retryable(), "unreachable is retryable");
    }

    fn socket_in_temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "control-radio-join-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("control.sock")
    }

    /// The whole CLI side over a real socket: the password goes from stdin into the one request
    /// line, and no refusal or result names it.
    #[test]
    fn it_sends_the_stdin_password_over_the_socket_and_never_echoes_it() {
        let path = socket_in_temp("ok");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let mut lines = Vec::new();
            for answer in [
                r#"{"ok":true,"data":{"server_host":"proximos-1a2b3c4d.local","message":"joined proximos-1a2b3c4d.local; restarting","reboot_in_s":5}}"#,
                r#"{"ok":false,"error":"the broker refused peer-proximos-5e6f7a8b: not authorized","error_code":"broker_refused"}"#,
            ] {
                let (stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                (&stream).write_all(answer.as_bytes()).unwrap();
                (&stream).write_all(b"\n").unwrap();
                lines.push(line);
            }
            lines
        });
        let socket = path.to_str().unwrap();
        let mut stdin = format!("{PASSWORD}\n");
        let joined = run(
            "proximos-1a2b3c4d.local",
            "fp",
            &mut stdin.as_bytes(),
            socket,
        )
        .unwrap();
        assert_eq!(joined["server_host"], "proximos-1a2b3c4d.local");
        assert!(!joined.to_string().contains(PASSWORD));

        stdin = format!("{PASSWORD}\n");
        let refused = run(
            "proximos-1a2b3c4d.local",
            "fp",
            &mut stdin.as_bytes(),
            socket,
        )
        .unwrap_err();
        assert_eq!(refused.kind(), Kind::Unauthenticated);
        assert!(!refused.message().contains(PASSWORD));

        let lines = server.join().unwrap();
        for line in lines {
            let sent: Value = serde_json::from_str(line.trim_end()).unwrap();
            assert_eq!(sent, request("proximos-1a2b3c4d.local", PASSWORD, "fp"));
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn no_agent_is_daemon_down_and_no_stdin_is_refused_before_dialling() {
        let path = socket_in_temp("none");
        let socket = path.to_str().unwrap();
        let e = run(
            "h.local",
            "fp",
            &mut format!("{PASSWORD}\n").as_bytes(),
            socket,
        )
        .unwrap_err();
        assert_eq!(e.kind(), Kind::DaemonDown);
        assert!(!e.message().contains(PASSWORD));

        // An empty stdin is refused before anything is dialled: the socket does not exist, and
        // the refusal is the stdin one, not DaemonDown.
        let e = run("h.local", "fp", &mut "".as_bytes(), socket).unwrap_err();
        assert_eq!(e.kind(), Kind::InvalidArgument);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
