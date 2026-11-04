//! The LoRaWAN arm — `plan/control/radio/lorawan.md`.
//!
//! ChirpStack is reached through exactly one client (§3: "Nothing else in the monorepo may
//! open a ChirpStack connection"), over native gRPC to `127.0.0.1:8080` — a loopback address
//! that stays deliberately non-configurable, carrying v1's deployment contract forward.

pub mod bootstrap;
pub mod client;
pub mod converge;
pub mod diagnostics;
pub mod enroll;
pub mod eui;
pub mod event;
pub mod fake;
pub mod gateway;
pub mod keyfile;
pub mod keys;
pub mod mirror;
pub mod model;
pub mod queue;
pub mod state;
pub mod token;
pub mod tonic_client;

pub use eui::Eui;
pub use state::{Activation, ConvergedState, DEFAULT_PROFILE};
pub use token::{Scope, Tokens};
