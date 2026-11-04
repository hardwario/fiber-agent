//! Generated protobuf types for the NODE protocol.
//!
//! Compiled by `build.rs` (prost-build) from `proto/app_config.proto`, copied
//! verbatim from sticker-firmware `app/src/app_config.proto` — the single
//! source of truth. Refreshed 2026-09-27 from 7be011e (feat-p2p, going into
//! v2.0.0 as #400; it had been a v1.4.0 copy). Covers `Telemetry` (fPort 2),
//! `AlarmReport` (fPort 3), `Command`/`Response` (fPort 85) and
//! `AppConfigMessage`.
//!
//! The schema declares no `package`, so prost emits the messages into the
//! crate-root file `_.rs`.
#![allow(clippy::all)]
#![allow(missing_docs)]

include!(concat!(env!("OUT_DIR"), "/_.rs"));
