//! Inbound (device -> portal) MQTT payload structs.
//!
//! Every struct is lenient: unknown fields are ignored (no
//! `deny_unknown_fields`), and every leaf documented as nullable in
//! `mqtt.md` is `Option<T>`. Where `mqtt.md` doesn't mark a field nullable
//! but the v1 Python handler (`app/mqtt/handlers.py`) reads it with
//! `dict.get(key)`/`dict.get(key, default)` rather than requiring it, this
//! crate follows the handler and makes the field `Option`/defaulted too —
//! Python's `.get()` never enforces presence the way serde's non-`Option`
//! fields do, so a literal transcription of "required" from the doc would
//! be stricter than the device contract has ever actually been. See each
//! module's doc comment for the specific handler lines this was checked
//! against.

pub mod alarm_event;
pub mod config_challenge;
pub mod config_response;
pub mod config_state;
pub mod errors;
pub mod export;
pub mod eye;
pub mod eye_aggregated;
pub mod lorawan;
pub mod pair_invite;
pub mod pair_response;
pub mod responses;
pub mod sensors_aggregated;
pub mod standby;
pub mod status;
pub mod system_info;
