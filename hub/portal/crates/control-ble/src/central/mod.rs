//! The central role: Beacon telemetry, provisioning and the EN12830 archive.
//! Design: `plan/control/ble/central.md`.

pub mod adapter;
pub mod aggregate;
pub mod cluster;
pub mod en12830;
pub mod enroll;
pub mod history;
pub mod ledger;
pub mod provision;
pub mod recording;
pub mod scan;
pub mod state;
