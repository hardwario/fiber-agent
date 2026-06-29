//! Generated protobuf types for the STICKER protocol.
//!
//! Compiled by `build.rs` (prost-build) from `proto/app_config.proto`, copied
//! verbatim from sticker-firmware v1.4.0 `app/src/app_config.proto` — the single
//! source of truth. Covers `Telemetry` (fPort 2), `AlarmReport` (fPort 3),
//! `Command`/`Response` (fPort 85) and `AppConfigMessage`.
//!
//! The schema declares no `package`, so prost emits the messages into the
//! crate-root file `_.rs`.
#![allow(clippy::all)]
#![allow(missing_docs)]

include!(concat!(env!("OUT_DIR"), "/_.rs"));
