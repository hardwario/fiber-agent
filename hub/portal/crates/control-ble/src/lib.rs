//! `control-ble` — the Hub's one Bluetooth component: Beacon telemetry, GATT provisioning and the
//! EN12830 archive (central role) and the FB00 GATT server the phone app uses (peripheral role).
//! Design: `plan/control/ble/{plan,central,peripheral,beacon-btsmp1}.md`.
//!
//! The public re-exports at the bottom are added by the task that first defines each type, so the
//! crate compiles at every commit: `error` (A2), `mode` (A4), `cli` (A5), `mac` (A8).

#![deny(unsafe_code)] // only central/en12830/att.rs opts back in, file-locally, for the raw L2CAP socket
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

pub mod central;
pub mod cli;
pub mod config;
pub mod decode;
pub mod dispatch;
pub mod error;
pub mod host;
pub mod mac;
pub mod mode;
pub mod peripheral;
pub mod status;
pub mod store;
pub mod tasks;

pub use cli::{Command, ControlBle, ControlBleCli};
pub use decode::{ParseError, Reading, TELTONIKA_COMPANY_ID};
pub use error::{Component, Error, ErrorDetail, Kind, Result, Role};
pub use mac::MacAddr;
pub use mode::{Mode, RoleArity};
