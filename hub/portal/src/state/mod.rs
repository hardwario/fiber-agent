//! In-memory portal state: the M1 work package (see plan/viewer/design.md
//! §4 and §7 "M1"). Owns the live `DeviceState`/`PortalState` model, the
//! `IngestEvent`/`ChangeEvent` contract wave-2 packages (MQTT handlers, SSE
//! hub) code against, and the `StateStore` that applies one to emit the
//! other.
//!
//! Nothing in this binary wires `StateStore` into the MQTT ingest task or
//! the HTTP/SSE layer yet — that lands with the parallel M1 packages that
//! own those files. Until then the public API is exercised only by this
//! module's own tests, which trips `dead_code` on an otherwise-intentional
//! public surface; suppressed here rather than left for wave-2 to clean up.
#![allow(dead_code, unused_imports)]

pub mod events;
pub mod models;
pub mod radio;
pub mod store;

pub use events::{ChangeEvent, IngestEvent, NodeConfigDumpMsg, NodeDeviceInfoMsg, NodeReadingMsg};
pub use models::{
    BeaconState, DeviceConfigState, DeviceState, MqttConnectionState, NodeConfigEntry,
    NodeDeviceInfoEntry, NodeFieldThreshold, NodeFullConfigEntry, NodeGatewayLink, NodeState,
    PortalState, SensorState, SensorThresholds,
};
pub use radio::{ApiStatus, GatewayStatus, GatewaysReport, RadioGatewaysState};
pub use store::StateStore;
