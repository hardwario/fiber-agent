//! Synchronous client for the control socket (used by `fiberctl`).
//!
//! One-shot request/response: connect, write one JSON line, read one JSON line,
//! close. Blocking std sockets — no async runtime needed in the CLI.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use super::protocol::{socket_path, Request, Response};

/// Send one request to the daemon's control socket and return its response.
pub fn send(req: &Request) -> Result<Response, String> {
    send_to(&socket_path(), req)
}

/// Like [`send`], but against an explicit socket path (used in tests).
pub fn send_to(path: &str, req: &Request) -> Result<Response, String> {
    let stream = UnixStream::connect(path).map_err(|e| {
        format!(
            "cannot connect to control socket {path}: {e}\n\
             (is the fiber_app daemon running with the control server enabled?)"
        )
    })?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    let mut line = serde_json::to_string(req).map_err(|e| format!("encode request: {e}"))?;
    line.push('\n');
    {
        let mut w = &stream;
        w.write_all(line.as_bytes()).map_err(|e| format!("write request: {e}"))?;
        w.flush().map_err(|e| format!("flush request: {e}"))?;
    }

    let mut reader = BufReader::new(&stream);
    let mut resp_line = String::new();
    let n = reader.read_line(&mut resp_line).map_err(|e| format!("read response: {e}"))?;
    if n == 0 {
        return Err("daemon closed the connection without responding".to_string());
    }
    serde_json::from_str(resp_line.trim_end()).map_err(|e| format!("decode response: {e}"))
}
