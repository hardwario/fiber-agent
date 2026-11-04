//! Key service (M4-B): device pairing identity (encrypted CA seed) and
//! per-user signing keys/certificates, wrapped at rest — the MDR-relevant
//! key custody layer under plan/v1/viewer/mqtt.md "Pairing" and rest.md
//! "Auth / PKI".
//!
//! Nothing here is wired into `http.rs`/`mqtt` yet — that's the M4-C
//! pairing/commands work package, coding against this module's public API
//! as a binding contract. Until then it's exercised only by its own tests,
//! which trips `dead_code`; suppressed here rather than left for M4-C
//! (same pattern as `src/auth/mod.rs`).
#![allow(dead_code, unused_imports)]

mod service;

pub use service::{
    DeviceIdentityInfo, KeyError, KeyService, UserSigningKey, PORTAL_BEACON_SERVICE_SIGNER,
    PORTAL_CLUSTER_SIGNER, PORTAL_SERVICE_SIGNER, PORTAL_SYNC_SIGNER,
};
