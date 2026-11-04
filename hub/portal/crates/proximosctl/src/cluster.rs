//! `proximosctl cluster` — a Server Hub's Client accounts (`plan/control/cluster.md` §7).
//!
//! `client-add` mints or rotates `peer-<hostname>` on this Hub's broker and registers the
//! Client's gateway; `client-remove` revokes both. The work is not done here: it runs in the
//! agent (`fiber_app`), the one root process, through the same apply path as the signed
//! `cluster-client-add` / `cluster-client-remove` the portal relays, so a registration made over
//! ssh and one made from the portal are the same registration. This is a client of the agent's
//! control socket (`/run/fiber/control.sock`, root only — the filesystem permission is the
//! authorization, decision #10), speaking its one-line JSON protocol.
//!
//! This crate does not depend on control-core (its BlueZ and hardware stack has no business in
//! a CLI), so the two request shapes are written out here, and both sides pin the same strings:
//! `the_requests_are_the_ones_the_agent_parses` below and control-core's
//! `the_cluster_requests_proximosctl_sends_parse`.
//!
//! **The password is printed once, on stdout, inside the result document**, and nowhere else: it
//! is not an argument, it is not logged, and the agent stores only its hash. The agent does not
//! hand it over in clear even here (plan/control/cluster.md §7, amended 2026-10-02). Each
//! `client-add` run makes its own ephemeral X25519 reply key and a request id, as the portal
//! does. The agent seals the password to them, and this command opens the seal in memory
//! (`fiber_protocol::sealed_reply`). It prints the document with `password` in place of
//! `password_sealed`. stdout is the terminal of whoever reached the root-only socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Subcommand};
use fiber_protocol::sealed_reply::{ReplySecret, Sealed};
use serde_json::{json, Value};

/// The agent's control socket, as control-core's `protocol::DEFAULT_SOCKET_PATH` names it.
pub const DEFAULT_SOCKET: &str = "/run/fiber/control.sock";
/// The same override control-core reads, so a dev agent on another path is reachable.
pub const SOCKET_ENV: &str = "FIBER_CONTROL_SOCKET";
/// control-core's `protocol::PROTOCOL_VERSION`.
const PROTOCOL_VERSION: u32 = 1;

/// Exit code of a partial registration: the document was printed — with the password, when the
/// account was minted — and something in it failed. Distinct from 1, "nothing was done", so a
/// script does not throw away a password that is live.
pub const EXIT_INCOMPLETE: u8 = 3;

/// A Server Hub's Client Hub accounts (plan/control/cluster.md §7).
///
/// Exit status: 0 the registration is complete; 3 it is incomplete — the result document is on
/// stdout, and so is the new password if the account was minted; 1 nothing was done (a refused
/// request, or no agent to ask).
#[derive(Debug, Args)]
pub struct Cluster {
    #[command(subcommand)]
    pub command: ClusterCommand,
}

#[derive(Debug, Subcommand)]
pub enum ClusterCommand {
    /// Mint (or, on a re-run, rotate) peer-<hostname> on this Hub's broker and register the
    /// Client's gateway. Prints the result, with the password, once.
    ClientAdd(ClientAddArgs),
    /// Remove peer-<hostname> and its ACL block — its sessions end at the reload — and
    /// deregister the gateway the account named.
    ClientRemove(ClientRemoveArgs),
}

#[derive(Debug, Args)]
pub struct ClientAddArgs {
    /// The Client Hub's hostname, proximos-<serial8>.
    #[arg(long)]
    pub hostname: String,
    /// The Client's LoRaWAN gateway EUI, 16 hex digits. Without it, or on a Hub whose radio
    /// runs no LoRaWAN, the gateway is skipped.
    #[arg(long)]
    pub gateway_eui: Option<String>,
}

#[derive(Debug, Args)]
pub struct ClientRemoveArgs {
    /// The Client Hub's hostname, proximos-<serial8>.
    #[arg(long)]
    pub hostname: String,
}

/// What one `client-add` run seals its answer to: its own reply secret, and the request id the
/// seal is bound to. Made fresh for every run and used once.
pub struct Reply {
    pub secret: ReplySecret,
    pub request_id: String,
}

impl Reply {
    pub fn generate() -> Reply {
        Reply {
            secret: ReplySecret::generate(),
            request_id: format!("cli-{}", uuid::Uuid::new_v4()),
        }
    }
}

/// The one request line for a command. `reply` is `client-add`'s, and is ignored for a removal.
pub fn request(command: &ClusterCommand, reply: Option<&Reply>) -> Value {
    let cmd = match command {
        ClusterCommand::ClientAdd(a) => {
            let mut cmd = json!({ "type": "cluster_client_add", "hostname": a.hostname });
            if let Some(eui) = &a.gateway_eui {
                cmd["gateway_eui"] = json!(eui);
            }
            if let Some(r) = reply {
                cmd["reply_key"] = json!(r.secret.reply_key());
                cmd["request_id"] = json!(r.request_id);
            }
            cmd
        }
        ClusterCommand::ClientRemove(a) => {
            json!({ "type": "cluster_client_remove", "hostname": a.hostname })
        }
    };
    json!({ "v": PROTOCOL_VERSION, "cmd": cmd })
}

/// One request, one response line, then the connection closes — control-core's
/// `control::client::send_to`, for this one shape.
pub fn send(path: &str, request: &Value) -> Result<Value, String> {
    let stream = UnixStream::connect(path).map_err(|e| {
        format!(
            "cannot reach the agent at {path}: {e} (is fiber.service running, and are you root?)"
        )
    })?;
    // The apply path waits for the broker to settle after its reload and may talk to ChirpStack.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let mut line = request.to_string();
    line.push('\n');
    (&stream)
        .write_all(line.as_bytes())
        .and_then(|()| (&stream).flush())
        .map_err(|e| format!("cannot send the request: {e}"))?;
    let mut answer = String::new();
    let n = BufReader::new(&stream)
        .read_line(&mut answer)
        .map_err(|e| format!("cannot read the answer: {e}"))?;
    if n == 0 {
        return Err("the agent closed the connection without answering".to_string());
    }
    serde_json::from_str(answer.trim_end()).map_err(|e| format!("cannot decode the answer: {e}"))
}

/// What to print and how to exit, for one answer.
#[derive(Debug, PartialEq)]
pub struct Rendered {
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub code: u8,
}

/// Opens `data.password_sealed` with this run's key and puts the password in its place, as
/// `password`. `Err` when there is a seal that does not open; the seal is then left out, and the
/// password is lost.
fn unseal(data: &mut Value, reply: Reply) -> Result<(), String> {
    let Some(map) = data.as_object_mut() else {
        return Ok(());
    };
    let Some(raw) = map.remove("password_sealed") else {
        return Ok(());
    };
    if raw.is_null() {
        return Ok(());
    }
    let hostname = map
        .get("hostname")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let sealed: Sealed = serde_json::from_value(raw)
        .map_err(|e| format!("the agent's password_sealed is malformed: {e}"))?;
    let password = reply
        .secret
        .open(&sealed, &reply.request_id, &hostname)
        .map_err(|e| format!("{e}; the account's password is lost, re-run to rotate it"))?;
    map.insert("password".to_string(), json!(password.as_str()));
    Ok(())
}

/// The agent's answer, as this command presents it. The result document goes to stdout whenever
/// there is one, on a partial result too, since it may hold the only copy of a live password.
/// `reply` is the key `client-add`'s answer was sealed to.
pub fn render(answer: &Value, reply: Option<Reply>) -> Rendered {
    let mut data = answer.get("data").filter(|d| !d.is_null()).cloned();
    let unsealed = match (data.as_mut(), reply) {
        (Some(d), Some(r)) => unseal(d, r),
        _ => Ok(()),
    };
    let stdout = data
        .as_ref()
        .map(|d| serde_json::to_string_pretty(d).unwrap_or_else(|_| d.to_string()));
    if let Err(e) = unsealed {
        return Rendered {
            stdout,
            stderr: Some(format!("proximosctl cluster: {e}")),
            code: EXIT_INCOMPLETE,
        };
    }
    if answer.get("ok").and_then(Value::as_bool) == Some(true) {
        return Rendered {
            stdout,
            stderr: None,
            code: 0,
        };
    }
    let error = answer
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("the agent refused without saying why");
    let incomplete = answer.get("error_code").and_then(Value::as_str) == Some("incomplete");
    Rendered {
        stdout,
        stderr: Some(format!("proximosctl cluster: {error}")),
        code: if incomplete { EXIT_INCOMPLETE } else { 1 },
    }
}

pub fn run(cli: &Cluster) -> ExitCode {
    let path = std::env::var(SOCKET_ENV).unwrap_or_else(|_| DEFAULT_SOCKET.to_string());
    let reply = matches!(cli.command, ClusterCommand::ClientAdd(_)).then(Reply::generate);
    let rendered = match send(&path, &request(&cli.command, reply.as_ref())) {
        Ok(answer) => render(&answer, reply),
        Err(e) => Rendered {
            stdout: None,
            stderr: Some(format!("proximosctl cluster: {e}")),
            code: 1,
        },
    };
    if let Some(out) = &rendered.stdout {
        println!("{out}");
    }
    if let Some(err) = &rendered.stderr {
        eprintln!("{err}");
    }
    ExitCode::from(rendered.code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// `fiber-protocol/vectors/sealed_reply.json`: a seal made by a third implementation.
    fn vector() -> Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../fiber-protocol/vectors/sealed_reply.json"
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// The vector's requester, under the vector's request id.
    fn vector_reply() -> Reply {
        let v = vector();
        let bytes: [u8; 32] = (0..32)
            .map(|i| {
                u8::from_str_radix(
                    &v["reply_secret_hex"].as_str().unwrap()[2 * i..2 * i + 2],
                    16,
                )
                .unwrap()
            })
            .collect::<Vec<u8>>()
            .try_into()
            .unwrap();
        Reply {
            secret: ReplySecret::from_bytes(bytes),
            request_id: v["request_id"].as_str().unwrap().to_string(),
        }
    }

    /// The strings control-core's `the_cluster_requests_proximosctl_sends_parse` parses.
    #[test]
    fn the_requests_are_the_ones_the_agent_parses() {
        let reply = Reply {
            request_id: "cli-0123456789abcdef".into(),
            ..vector_reply()
        };
        let add = request(
            &ClusterCommand::ClientAdd(ClientAddArgs {
                hostname: "proximos-5e6f7a8b".into(),
                gateway_eui: Some("0016c001ff10a235".into()),
            }),
            Some(&reply),
        );
        assert_eq!(
            add,
            serde_json::from_str::<Value>(
                r#"{"v":1,"cmd":{"type":"cluster_client_add","hostname":"proximos-5e6f7a8b","gateway_eui":"0016c001ff10a235","reply_key":"1wWpoC1yfr-Mw9EeK1ML134Q3CxHu5Otu_l1wzNrSy0","request_id":"cli-0123456789abcdef"}}"#
            )
            .unwrap()
        );
        let bare = request(
            &ClusterCommand::ClientAdd(ClientAddArgs {
                hostname: "proximos-5e6f7a8b".into(),
                gateway_eui: None,
            }),
            Some(&reply),
        );
        assert_eq!(
            bare,
            serde_json::from_str::<Value>(
                r#"{"v":1,"cmd":{"type":"cluster_client_add","hostname":"proximos-5e6f7a8b","reply_key":"1wWpoC1yfr-Mw9EeK1ML134Q3CxHu5Otu_l1wzNrSy0","request_id":"cli-0123456789abcdef"}}"#
            )
            .unwrap()
        );
        let remove = request(
            &ClusterCommand::ClientRemove(ClientRemoveArgs {
                hostname: "proximos-5e6f7a8b".into(),
            }),
            None,
        );
        assert_eq!(
            remove,
            serde_json::from_str::<Value>(
                r#"{"v":1,"cmd":{"type":"cluster_client_remove","hostname":"proximos-5e6f7a8b"}}"#
            )
            .unwrap()
        );
    }

    /// Every run makes its own key and request id.
    #[test]
    fn each_run_has_its_own_reply_key() {
        let (a, b) = (Reply::generate(), Reply::generate());
        assert_ne!(a.secret.reply_key(), b.secret.reply_key());
        assert_ne!(a.request_id, b.request_id);
        assert!(a.request_id.starts_with("cli-"));
    }

    /// The vector's answer, as the agent would send it.
    fn sealed_doc(gateway: &str) -> Value {
        let v = vector();
        json!({
            "hostname": v["hostname"], "username": format!("peer-{}", v["hostname"].as_str().unwrap()),
            "gateway_eui": null, "account": "ok", "gateway": gateway,
            "password_sealed": v["sealed"], "message": "account minted",
        })
    }

    #[test]
    fn answers_map_to_output_and_exit_codes_with_the_password_opened() {
        let password = vector()["password"].as_str().unwrap().to_string();
        let ok = render(
            &json!({"ok": true, "data": sealed_doc("skipped")}),
            Some(vector_reply()),
        );
        assert_eq!(ok.code, 0, "{ok:?}");
        let out: Value = serde_json::from_str(ok.stdout.as_deref().unwrap()).unwrap();
        assert_eq!(
            out["password"],
            password.as_str(),
            "opened in memory and printed"
        );
        assert!(out.get("password_sealed").is_none());
        assert!(ok.stderr.is_none());

        let partial = render(
            &json!({
                "ok": false, "data": sealed_doc("failed"),
                "error": "registration incomplete: gateway failed: ChirpStack is down",
                "error_code": "incomplete"
            }),
            Some(vector_reply()),
        );
        assert_eq!(partial.code, EXIT_INCOMPLETE);
        assert!(
            partial.stdout.unwrap().contains(&password),
            "the live password is kept"
        );
        assert!(partial.stderr.unwrap().contains("ChirpStack is down"));

        let refused = render(
            &json!({
                "ok": false, "data": null, "error": "hostname \"x\" is not a Hub hostname",
                "error_code": "validation"
            }),
            Some(vector_reply()),
        );
        assert_eq!(refused.code, 1);
        assert!(refused.stdout.is_none());
    }

    /// A seal that does not open with this run's key prints no password and says it is lost.
    #[test]
    fn a_seal_for_another_key_or_request_does_not_open() {
        let password = vector()["password"].as_str().unwrap().to_string();
        for reply in [
            Reply::generate(),
            Reply {
                request_id: "cli-another".into(),
                ..vector_reply()
            },
        ] {
            let r = render(&json!({"ok": true, "data": sealed_doc("ok")}), Some(reply));
            assert_eq!(r.code, EXIT_INCOMPLETE);
            assert!(!r.stdout.as_deref().unwrap_or_default().contains(&password));
            assert!(
                !r.stdout
                    .as_deref()
                    .unwrap_or_default()
                    .contains("password_sealed"),
                "{r:?}"
            );
            assert!(r.stderr.unwrap().contains("re-run"));
        }
    }

    /// One line out, one line back, over a real socket.
    #[test]
    fn it_speaks_the_one_line_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            (&stream)
                .write_all(b"{\"ok\":true,\"data\":{\"account\":\"ok\"}}\n")
                .unwrap();
            line
        });
        let answer = send(
            path.to_str().unwrap(),
            &request(
                &ClusterCommand::ClientRemove(ClientRemoveArgs {
                    hostname: "proximos-5e6f7a8b".into(),
                }),
                None,
            ),
        )
        .unwrap();
        assert_eq!(answer["data"]["account"], "ok");
        let sent: Value = serde_json::from_str(server.join().unwrap().trim_end()).unwrap();
        assert_eq!(sent["cmd"]["type"], "cluster_client_remove");

        let missing = dir.path().join("nobody-here.sock");
        assert!(send(missing.to_str().unwrap(), &json!({})).is_err());
    }
}
