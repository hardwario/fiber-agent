//! Multi-destination store-and-forward MQTT exporter for the save-and-feed
//! pipeline. The firmware DB (`sticker_readings`, `sensor_readings`,
//! `alarm_events`) is the authoritative store; this module drains rows
//! past per-(broker_id, stream) cursors and publishes them at QoS 1.

pub mod config;
pub mod drain;
pub mod envelope;

pub use config::{DestinationConfig, ExportConfig, TlsConfig};
pub use drain::{drain_one_batch, DrainConfig, Publisher, Stream};
pub use envelope::{alarm_envelope, probe_envelope, sticker_envelope};
