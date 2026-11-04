//! `control_radio` — the radio control library behind `proximosctl control.radio`.
//!
//! Written clean-room from the plan set: `plan/control/radio/{plan,lorawan,p2p,
//! node-firmware-v1.4.0}.md`, plus `plan/portal/control.radio.md` and
//! `plan/system/{design.md,lora/design.md}` for the contracts they own. No v1 source is an
//! input; where a v1 behavior is the contract, the plan states it and this code implements
//! the plan.
//!
//! # Shape
//!
//! Decision #12 makes this a **crate inside the one `portal/` workspace**, not a top-level
//! component and not a daemon: the three loop duties that the plan gave to
//! `control-radio.service` — the ChirpStack event subscription feeding the decode path, the
//! 60 s `radio/gateways` health poll, and the `node-get` cache — are exposed here as
//! spawnable tasks for `proximos-control` to own, and `/run/proximos/control-radio.sock`
//! dissolves into the one control socket.
//!
//! Everything the plan set decided on its own terms survives: the [`mode`] dispatch gate, the
//! [`config`] schema and decoder registry, the `control.radio` command surface and exit-code
//! table, the transport-agnostic [`error`] taxonomy, `radio-setup.service` and the per-arm
//! provisioning markers.
//!
//! # Standalone guarantee
//!
//! This crate depends on no other crate in the workspace, and its integration stack runs with
//! no agent subsystems started. `tests/standalone.rs` asserts the first half; the
//! `control-radio-harness` binary is the second.

#![forbid(unsafe_code)]
// stack.md Rules #3: no unwrap()/expect() outside tests. Enforced here rather than left to
// review, since clippy runs with -D warnings in CI.
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

/// The version this build reports — `/health`, the page footer, `proximosctl --version`, the
/// BLE device info: the image's release version (`plan/system/design.md` §9.1), which
/// `fiber-portal.bb` passes as `PROXIMOS_VERSION` at compile time on a release-tag build only,
/// and this crate's own version everywhere else (a developer's `cargo build`, an untagged
/// image).
pub const VERSION: &str = match option_env!("PROXIMOS_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

pub mod cache;
pub mod cli;
pub mod cluster_join;
pub mod cluster_role;
pub mod coalesce;
pub mod config;
pub mod decode;
pub mod dispatch;
pub mod error;
pub mod lorawan;
pub mod mode;
pub mod mqtt;
pub mod node_move;
pub mod northbound;
pub mod p2p;
pub mod registry;
pub mod setmode;
pub mod status;
pub mod store;
pub mod tasks;

pub use cli::{Command, ControlRadio, ControlRadioCli};
pub use error::{Component, Error, ErrorDetail, Kind, Result, Transport};
pub use mode::{Hardware, Mode, RadioArity};
