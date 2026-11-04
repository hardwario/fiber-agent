//! `fiber-protocol` — the typed MQTT/signed-command contract between the
//! ProXimos v2 portal and FIBER gateways.
//!
//! This is a **vendored copy** of the device contract, hand-ported from
//! the v1 Python viewer (`proximos-v1/viewer/backend/app/mqtt/`,
//! `app/auth/command_signer.py`) against the inventory in
//! `proximos-v2/plan/v1/viewer/mqtt.md`. It is meant to be replaced by a
//! crate shared with `application/` (the Rust device agent) once that
//! extraction is agreed with the agent's owner — see
//! `plan/viewer/stack.md` rule #1 and `plan/viewer/design.md` §8. Until
//! then, this crate is the single source of truth for the portal side and
//! any drift from the agent must be caught by the golden-diff harness
//! (design.md §6), not assumed away.
//!
//! # Modules
//!
//! - [`topics`] — MQTT topic builder (subscribe list in load-bearing
//!   order, outbound command topics, hostname extraction).
//! - [`payloads`] — serde structs for every inbound payload documented in
//!   mqtt.md. Lenient by construction: no `deny_unknown_fields`, and every
//!   field the wire format may omit or null out is `Option`.
//! - [`envelope`] — the Ed25519 signed-command envelope v2
//!   (`config_request`/`config_confirm`, carrying a signed `v` and
//!   `level`), including the canonical-JSON pre-image serialization that
//!   is byte-exact with the device agent's verifier
//!   (`control-core/src/libs/authorization/manager.rs`). The exact strings
//!   and signatures are checked in as `vectors/envelope_v2.json` and
//!   consumed by all three implementations (design.md §10).
//! - [`certificate`] — the per-signer certificate minted by the device CA
//!   and attached to signed commands, mirroring the agent's
//!   `UserCertificate` (`application/src/libs/crypto/certificate.rs`).
//! - [`pairing`] — decrypting the `pair/response` payload (PBKDF2 +
//!   AES-256-GCM) into the raw Ed25519 seed, mirroring the agent's
//!   `application/src/libs/pairing/crypto.rs`. Also owns
//!   [`LOCAL_PAIRING_PASSWORD`], the fixed KDF password that replaces the
//!   typed code when the portal pairs its own hub.
//! - [`registry`] — the command registry: the required permission and the
//!   [`Level`] of every `command_type`, looked up instead of routed by
//!   fragile set-membership (mqtt.md quirk #3). One dialect, one topic —
//!   the v1 unsigned flat-dialect verbs are ordinary signed rows here
//!   (plan/portal/mqtt/design.md §4, §7).
//! - [`sealed_reply`] — the requester's half of `cluster-client-add`'s
//!   sealed password: the per-request X25519 reply key and the open
//!   (X25519 + HKDF-SHA256 + AES-256-GCM), pinned by
//!   `vectors/sealed_reply.json` (plan/control/cluster.md §7).
//! - [`display`] — the `set_display_lines` payload type
//!   ([`DisplayLine`]) and its validator, shared with the agent's own copy
//!   in `control-core` (plan/portal/display-config.md WP-B/B1).
//! - [`fields`] — LoRaWAN/EYE field registries and Node
//!   (`STICKER_SETTABLE`/`STICKER_READ_ONLY`) config-key validation,
//!   cross-checked against the device agent's own
//!   `application/src/libs/lorawan/{registry,sticker_command}.rs`
//!   (ws.md/rest.md quirk #9: one registry hand-mirrored in
//!   Rust/Python/TypeScript, guarded only by convention).
//! - [`grading`] — the one threshold grading rule with hysteresis, run by
//!   the portal and by the Hub (plan/control/alarm_hysteresis.md).
//! - [`config_sync`] — Portal -> Hub config sync topics and desired
//!   document schema (plan/portal/config_sync.md §5, decision #43).

pub mod certificate;
pub mod config_sync;
pub mod display;
pub mod envelope;
pub mod fields;
pub mod grading;
pub mod pairing;
pub mod payloads;
pub mod registry;
pub mod sealed_reply;
pub mod topics;

pub use certificate::{
    sign_certificate, verify_certificate, Certificate, CertificateError, CertificatePayload,
};
pub use config_sync::{
    document_hash, is_desired_verb, parse_sync_topic, DesiredDocument, SyncAck, SyncHint, SyncItem,
    SyncItemResult, SyncRequest, SyncResponse, SyncTarget, SyncTopic, DESIRED_VERBS,
    MAX_RESP_BYTES,
};
pub use display::{
    validate_display_custom_lines, validate_display_line, DisplayLine, DisplayLineFormat,
    DisplayLineSource,
};
pub use envelope::{
    build_confirm, build_request, canonical_json, sign_confirm, sign_request, verify_confirm,
    verify_request, EnvelopeError, SignedConfirm, SignedRequest, ENVELOPE_VERSION,
};
pub use pairing::{decrypt_pair_response, derive_public_hex, PairingError, LOCAL_PAIRING_PASSWORD};
pub use registry::{lookup, CommandSpec, Level};
pub use topics::{extract_hostname, TopicBuilder};

pub use payloads::alarm_event::{AlarmEventMsg, AlarmType};
pub use payloads::config_challenge::ConfigChallengeMsg;
pub use payloads::config_response::ConfigResponseMsg;
pub use payloads::config_state::{
    ConfigStateMsg, EyeConfig, EyeTagConfigEntry, Intervals, LorawanSensorConfigEntry,
    SensorConfigEntry, SensorThresholds,
};
pub use payloads::errors::DeviceErrorMsg;
pub use payloads::export::{
    ExportEnvelope, EyeExportData, EyeExportPayload, Probe1mData, Probe1mReplayData,
    StickerExportData,
};
pub use payloads::eye::{EyeSensorsMsg, EyeTag};

pub use fields::Quantity;

pub use fields::eye::{
    eye_field, eye_registry, is_known as eye_field_is_known,
    is_thresholdable as eye_field_is_thresholdable, EyeFieldDef,
};
pub use fields::lorawan::{
    lorawan_field, lorawan_registry, FieldDef as LorawanFieldDef, Group as LorawanFieldGroup,
    Kind as LorawanFieldKind, ThresholdKind as LorawanThresholdKind,
};
pub use fields::sticker_config::{
    validate_sticker_config, KeyError as StickerConfigKeyError, KeySpec as StickerConfigKeySpec,
    READ_ONLY as STICKER_READ_ONLY, SETTABLE as STICKER_SETTABLE,
};
pub use payloads::lorawan::{
    ExternalGateway, FieldThreshold, LorawanEvent, LorawanGatewayRssi, LorawanGatewaysMsg,
    LorawanSensor, LorawanSensorsMsg, NodeCommandResultMsg, NodeConfigMsg, NodeFullConfigMsg,
    NodeHistoryMsg, NodeHistoryRecord, NodeInfoMsg, NodeLastAck,
};
pub use payloads::pair_invite::PairInviteMsg;
pub use payloads::pair_response::{EncryptedPrivateKey, PairResponseMsg};
pub use payloads::responses::{HistoryResponse, IntervalConfigResponse, SensorConfigResponse};
pub use payloads::sensors_aggregated::{
    AggregatedSensor, AlarmCounts, SensorsAggregatedMsg, TemperatureStats,
};
pub use payloads::standby::StandbyMsg;
pub use payloads::status::{parse_status, DeviceStatus, StatusMsg};
pub use payloads::system_info::{
    Battery, Dc, Ethernet, Hardware, LorawanGatewayInfo, Network, Power, SetupInfo, Storage,
    SystemInfoMsg, Wifi,
};
