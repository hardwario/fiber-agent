//! Robustness / concurrency / lifecycle tests for the control socket (#79).
//!
//! Complements `control_cli.rs` (happy-path e2e via the compiled binary) by
//! hammering the *real* `serve()`/`dispatch()` (the same code `main` runs) with
//! the adversarial inputs a daemon must survive: concurrent clients, oversized /
//! partial / empty / non-UTF8 requests, stale-socket recovery, and socket
//! permissions. The real-daemon-on-device path is unavailable here (aarch64
//! cross toolchain + native C deps), so this is the highest-fidelity layer that
//! runs on the host.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use fiber_app::libs::config::Config;
use fiber_app::libs::control::client;
use fiber_app::libs::control::protocol::{Command, Request, Response};
use fiber_app::libs::control::server::{serve, ControlContext};

/// Spawn the real server on a unique temp socket; return (tempdir, path).
fn start_server() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock").to_string_lossy().to_string();
    let ctx = ControlContext::new(
        "robust-test".to_string(),
        Arc::new(Config::default_config()),
        None,
        None,
        Duration::from_millis(200),
    );
    let p = path.clone();
    std::thread::spawn(move || {
        let _ = serve(ctx, &p);
    });
    for _ in 0..300 {
        if client::send_to(&path, &Request::new(Command::Status)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    (dir, path)
}

/// Raw request: write exactly `bytes` to the socket, return the raw reply (or
/// empty if the server closed without one). Never panics on a broken pipe.
fn raw_exchange(path: &str, bytes: &[u8]) -> Vec<u8> {
    let stream = UnixStream::connect(path).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    {
        let mut w = &stream;
        // a broken pipe here is itself a valid outcome for some cases
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut buf = Vec::new();
    let _ = (&stream).take(128 * 1024).read_to_end(&mut buf);
    buf
}

// --- concurrency ---

#[test]
fn many_concurrent_clients_all_succeed() {
    let (_d, path) = start_server();
    let mut handles = Vec::new();
    for _ in 0..32 {
        let p = path.clone();
        handles.push(std::thread::spawn(move || {
            let r = client::send_to(&p, &Request::new(Command::Status)).unwrap();
            assert!(r.ok, "concurrent status failed: {:?}", r.error);
            assert_eq!(r.data["app_version"], "robust-test");
        }));
    }
    for h in handles {
        h.join().expect("client thread panicked");
    }
}

#[test]
fn concurrent_fiberctl_processes() {
    let (_d, path) = start_server();
    let bin = env!("CARGO_BIN_EXE_fiberctl");
    let mut kids = Vec::new();
    for _ in 0..8 {
        let p = path.clone();
        let bin = bin.to_string();
        kids.push(std::thread::spawn(move || {
            std::process::Command::new(bin)
                .args(["--socket", &p, "status"])
                .output()
                .expect("run fiberctl")
                .status
                .success()
        }));
    }
    for k in kids {
        assert!(k.join().unwrap(), "a concurrent fiberctl invocation failed");
    }
}

#[test]
fn many_sequential_requests_no_degradation() {
    // guards against fd leaks / accept-loop wedging over many one-shot conns
    let (_d, path) = start_server();
    for i in 0..200 {
        let r = client::send_to(&path, &Request::new(Command::Status)).unwrap();
        assert!(r.ok, "request {i} failed: {:?}", r.error);
    }
}

// --- malformed / adversarial input; server must survive each ---

fn parse_reply(buf: &[u8]) -> Option<Response> {
    let line = String::from_utf8_lossy(buf);
    serde_json::from_str(line.trim()).ok()
}

#[test]
fn oversized_request_is_rejected_not_oom() {
    let (_d, path) = start_server();
    // 100 KiB with no newline: the 64 KiB read cap trips, parse fails.
    let mut payload = vec![b'x'; 100 * 1024];
    payload.push(b'\n');
    let reply = raw_exchange(&path, &payload);
    if let Some(r) = parse_reply(&reply) {
        assert!(!r.ok, "oversized junk should not be ok");
    }
    // crucially, the server is still alive afterwards:
    assert!(client::send_to(&path, &Request::new(Command::Status)).unwrap().ok);
}

#[test]
fn empty_line_request() {
    let (_d, path) = start_server();
    let reply = raw_exchange(&path, b"\n");
    let r = parse_reply(&reply).expect("server should reply to empty line");
    assert!(!r.ok);
    assert_eq!(r.error_code.as_deref(), Some("bad_request"));
}

#[test]
fn connection_closed_without_newline_is_handled() {
    let (_d, path) = start_server();
    // write partial bytes, no newline, then close the write half
    let _ = raw_exchange(&path, b"{\"v\":1,\"cmd\"");
    // server must not crash; next request still works
    assert!(client::send_to(&path, &Request::new(Command::Status)).unwrap().ok);
}

#[test]
fn non_utf8_request_does_not_crash_server() {
    let (_d, path) = start_server();
    // invalid UTF-8; read_line errors -> connection dropped, server survives
    let _ = raw_exchange(&path, &[0xff, 0xfe, 0x00, 0x80, b'\n']);
    assert!(client::send_to(&path, &Request::new(Command::Status)).unwrap().ok);
}

#[test]
fn connect_then_immediately_close_is_handled() {
    let (_d, path) = start_server();
    // open + close with nothing sent
    for _ in 0..10 {
        let s = UnixStream::connect(&path).unwrap();
        drop(s);
    }
    assert!(client::send_to(&path, &Request::new(Command::Status)).unwrap().ok);
}

#[test]
fn garbage_then_valid_on_separate_connections() {
    let (_d, path) = start_server();
    let bad = parse_reply(&raw_exchange(&path, b"not json at all\n")).unwrap();
    assert_eq!(bad.error_code.as_deref(), Some("bad_request"));
    let good = client::send_to(&path, &Request::new(Command::Status)).unwrap();
    assert!(good.ok);
}

// --- socket lifecycle ---

#[test]
fn socket_and_parent_have_locked_down_perms() {
    let (_d, path) = start_server();
    let sock_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(sock_mode, 0o600, "socket must be owner-only");
    let parent = std::path::Path::new(&path).parent().unwrap();
    let dir_mode = std::fs::metadata(parent).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "control dir must be owner-only");
}

#[test]
fn serve_recovers_over_a_stale_socket_file() {
    // pre-create a leftover file where the socket should bind
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock").to_string_lossy().to_string();
    std::fs::write(&path, b"stale").unwrap();
    let ctx = ControlContext::new(
        "stale-test".to_string(),
        Arc::new(Config::default_config()),
        None,
        None,
        Duration::from_millis(200),
    );
    let p = path.clone();
    std::thread::spawn(move || {
        let _ = serve(ctx, &p);
    });
    let mut ok = false;
    for _ in 0..300 {
        if let Ok(r) = client::send_to(&path, &Request::new(Command::Status)) {
            ok = r.ok;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ok, "server should remove the stale socket and bind");
}

#[test]
fn interleaved_raw_and_typed_clients() {
    // a malformed raw client between two good clients must not corrupt state
    let (_d, path) = start_server();
    assert!(client::send_to(&path, &Request::new(Command::Status)).unwrap().ok);
    let _ = raw_exchange(&path, b"{ broken \n");
    let mut reader_threads = Vec::new();
    for _ in 0..8 {
        let p = path.clone();
        reader_threads.push(std::thread::spawn(move || {
            client::send_to(&p, &Request::new(Command::ConfigGet { key: "system.app_version".into() }))
                .unwrap()
                .ok
        }));
    }
    for t in reader_threads {
        assert!(t.join().unwrap());
    }
}
