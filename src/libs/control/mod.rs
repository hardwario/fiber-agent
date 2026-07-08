//! Control plane for the FIBER application (#79).
//!
//! - [`protocol`] — the versioned newline-delimited JSON request/response types,
//!   shared by the daemon (server) and the `fiberctl` client.
//! - [`server`] — the in-daemon Unix-socket server ([`server::serve`]) that
//!   dispatches to the live subsystem handles ([`server::ControlContext`]).
//! - [`client`] — the synchronous one-shot client used by `fiberctl`.

pub mod client;
pub mod protocol;
pub mod server;
