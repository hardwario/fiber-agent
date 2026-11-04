//! The transport-agnostic error taxonomy — `plan/control/radio/plan.md` §6.
//!
//! One enum, surfaced identically through the CLI (exit codes + `--json`), the control
//! socket, and `portal`. Every variant has a defined meaning under *both* transports; the
//! transport-specific diagnosis travels in [`ErrorDetail`], never in a new variant. That is
//! what keeps the exit-code table stable as transports are added.

use std::fmt;

/// Which dispatch arm produced an error. Serialized into the structured detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Lorawan,
    P2p,
}

impl Transport {
    /// The wire spelling. `p2p` and `lorawan` are the two `--radio` values.
    pub const fn as_str(self) -> &'static str {
        match self {
            Transport::Lorawan => "lorawan",
            Transport::P2p => "p2p",
        }
    }
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The closed `component` vocabulary — §6 "Structured detail".
///
/// Decision #6 requires a declared surface, so this is an enum rather than a string:
/// extended only by adding a variant, never free-form. A `ServiceDown` alone does not say
/// whether to check `chirpstack.service` or the northbridge UART; this is what does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Component {
    ChirpstackApi,
    ChirpstackService,
    TokenFile,
    ConfigJson,
    EnrollmentRecord,
    MqttBroker,
    NorthbridgeUart,
    NorthbridgeFirmware,
    SessionDb,
    NodeRegistry,
    ControlSocket,
    /// The `radio.mode` dispatch gate itself — the four mode/scope refusals below, which are
    /// decided from local config before any RPC or socket connect.
    RadioMode,
}

impl Component {
    pub const fn as_str(self) -> &'static str {
        match self {
            Component::ChirpstackApi => "chirpstack-api",
            Component::ChirpstackService => "chirpstack-service",
            Component::TokenFile => "token-file",
            Component::ConfigJson => "config-json",
            Component::EnrollmentRecord => "enrollment-record",
            Component::MqttBroker => "mqtt-broker",
            Component::NorthbridgeUart => "northbridge-uart",
            Component::NorthbridgeFirmware => "northbridge-firmware",
            Component::SessionDb => "session-db",
            Component::NodeRegistry => "node-registry",
            Component::ControlSocket => "control-socket",
            Component::RadioMode => "radio-mode",
        }
    }

    /// The whole closed vocabulary, in declaration order. Used by the schema test that
    /// asserts every emitted error draws its component from this set.
    pub const ALL: [Component; 12] = [
        Component::ChirpstackApi,
        Component::ChirpstackService,
        Component::TokenFile,
        Component::ConfigJson,
        Component::EnrollmentRecord,
        Component::MqttBroker,
        Component::NorthbridgeUart,
        Component::NorthbridgeFirmware,
        Component::SessionDb,
        Component::NodeRegistry,
        Component::ControlSocket,
        Component::RadioMode,
    ];
}

impl fmt::Display for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error variants — §6's table, in its order.
///
/// `Kind` carries no payload so it can map 1:1 to an exit code and be compared in tests;
/// the payload lives in [`Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    /// A required token file absent — bootstrap never ran or was wiped. P2P: central-service
    /// state never initialized, `create --radio p2p` never ran.
    NotBootstrapped,
    /// Token *rejected* — revoked row, rotated `api.secret`, wiped DB. Distinct from
    /// `NotBootstrapped`: the credential exists, the far side refuses it.
    Unauthenticated,
    /// Token valid but under-scoped. P2P: reserved for the future `node-info` ACL.
    PermissionDenied,
    /// Unknown `dev_eui`/gateway EUI, or a `--serial` with no enrollment-record mapping.
    NotFound,
    /// Duplicate resource on converge. P2P: a duplicate registration — `node-add --radio p2p`
    /// for a `serial` already in the node registry.
    AlreadyExists,
    /// Malformed EUI, key material, or parameter. Also the CLI usage error.
    InvalidArgument,
    /// Connection refused — chirpstack not listening. P2P: northbridge not responding, or
    /// running firmware without the radio protocol (p2p.md §0.1).
    ServiceDown,
    /// Deadline exceeded. "service active" is not "API ready" at boot.
    Timeout,
    /// Local I/O under `/data/proximos/radio/` failed. Distinct from `NotBootstrapped`: the
    /// state exists but cannot be used.
    StorageError,
    /// Residual wire error — anything else on the gRPC wire or the UART/LAN leg.
    Transport,
    /// External-gateway op while the gateway-bridge opt-in flag is off.
    BridgeDisabled,
    /// A socket-routed subcommand with the agent stopped.
    DaemonDown,
    /// A LoRaWAN command arrived while `radio.mode: p2p`.
    LorawanUnavailable,
    /// A P2P command arrived while `radio.mode: lorawan`.
    P2pUnavailable,
    /// `--radio` omitted while `radio.mode: dual`.
    RadioParameterRequired,
    /// Needs a P2P Client Hub, or a P2P gateway op, neither of which exists in phase 1. (A
    /// LoRaWAN Client, `create --role client`, exists since plan/control/cluster.md C4.)
    Phase2NotImplemented,
    /// Any radio command while `radio.mode: off`.
    ///
    /// Deliberately distinct from the `*Unavailable` pair, which mean "the other arm is the
    /// active one". `off` means no arm is enabled at all, and the remediation differs: this
    /// Hub was provisioned with the radio disabled, rather than the caller naming the wrong
    /// transport. Added 2026-08-21 (`plan.md` §6, MR !14 finding 2.5, overruled) — the table
    /// is additive-only, so a new meaning takes a new number rather than widening an old one.
    OffMode,
    /// Two P2P Nodes whose DevEUIs share their low 32 bits, the TOWER address
    /// (`p2p_tower_gateway.md` §3.1): `node-add --radio p2p` or `node-move --to p2p` of the
    /// second one. A TOWER gateway keys its peers by that address and cannot serve both.
    /// Added 2026-09-28 (`plan.md` §6); no LoRaWAN meaning — ChirpStack assigns addresses.
    AddressCollision,
}

impl Kind {
    /// The wire spelling, which is also the `error` field of the structured detail.
    ///
    /// Note `LoRaWANUnavailable` — the plan spells the variant with LoRaWAN's own casing,
    /// and this string is the consumed contract, so it is written out rather than derived
    /// from the Rust identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Kind::NotBootstrapped => "NotBootstrapped",
            Kind::Unauthenticated => "Unauthenticated",
            Kind::PermissionDenied => "PermissionDenied",
            Kind::NotFound => "NotFound",
            Kind::AlreadyExists => "AlreadyExists",
            Kind::InvalidArgument => "InvalidArgument",
            Kind::ServiceDown => "ServiceDown",
            Kind::Timeout => "Timeout",
            Kind::StorageError => "StorageError",
            Kind::Transport => "Transport",
            Kind::BridgeDisabled => "BridgeDisabled",
            Kind::DaemonDown => "DaemonDown",
            Kind::LorawanUnavailable => "LoRaWANUnavailable",
            Kind::P2pUnavailable => "P2PUnavailable",
            Kind::RadioParameterRequired => "RadioParameterRequired",
            Kind::Phase2NotImplemented => "Phase2NotImplemented",
            Kind::OffMode => "OffMode",
            Kind::AddressCollision => "AddressCollision",
        }
    }

    /// The assigned exit code — §6 "Exit codes — the assigned table (contract)".
    ///
    /// Banded so scripts can branch on ranges: 0 success, 2 usage, 10-19 config facts (fail
    /// fast), 20-29 transient (retry with backoff — the band `radio-setup`'s boot retry
    /// policy keys on), 30-39 mode/scope refusals (fail fast), 40+ storage.
    ///
    /// Additive-only: a new variant gets a new number, existing numbers never change
    /// meaning. `tests/exit_codes.rs` pins every row.
    pub const fn exit_code(self) -> i32 {
        match self {
            Kind::InvalidArgument => 2,
            Kind::NotBootstrapped => 10,
            Kind::Unauthenticated => 11,
            Kind::PermissionDenied => 12,
            Kind::NotFound => 13,
            Kind::AlreadyExists => 14,
            // 15 is control.ble's `PairingRejected`; the two groups share one table.
            Kind::AddressCollision => 16,
            Kind::ServiceDown => 20,
            Kind::Timeout => 21,
            Kind::Transport => 22,
            Kind::DaemonDown => 23,
            Kind::BridgeDisabled => 30,
            Kind::LorawanUnavailable => 31,
            Kind::P2pUnavailable => 32,
            Kind::RadioParameterRequired => 33,
            Kind::Phase2NotImplemented => 34,
            Kind::OffMode => 35,
            Kind::StorageError => 40,
        }
    }

    /// True for the transient band. `radio-setup.service` retries these with backoff up to a
    /// deadline (chirpstack may still be migrating its DB on first boot) and fails fast on
    /// everything else.
    pub const fn is_retryable(self) -> bool {
        let c = self.exit_code();
        c >= 20 && c <= 29
    }

    /// Every variant, in §6 table order — the fixture the mapping tests walk.
    pub const ALL: [Kind; 18] = [
        Kind::NotBootstrapped,
        Kind::Unauthenticated,
        Kind::PermissionDenied,
        Kind::NotFound,
        Kind::AlreadyExists,
        Kind::InvalidArgument,
        Kind::ServiceDown,
        Kind::Timeout,
        Kind::StorageError,
        Kind::Transport,
        Kind::BridgeDisabled,
        Kind::DaemonDown,
        Kind::LorawanUnavailable,
        Kind::P2pUnavailable,
        Kind::RadioParameterRequired,
        Kind::Phase2NotImplemented,
        Kind::OffMode,
        Kind::AddressCollision,
    ];
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The mandatory structured detail — §6.
///
/// This is the serialized shape, identical in `--json`, the control-socket response and the
/// `portal` MQTT/WS event. A bare variant name is never the whole story, so constructing an
/// [`Error`] without a component is not possible.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ErrorDetail {
    /// Enum variant — maps 1:1 to the exit code. Scripts branch on this.
    pub error: Kind,
    /// Which dispatch arm produced it. `None` for a failure raised before dispatch picked an
    /// arm (a malformed global flag, an unreadable config file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    /// Closed vocabulary. Never free-form.
    pub component: Component,
    /// Human-readable, for logs and `portal` display. **Never for branching.**
    pub message: String,
}

/// A `control.radio` error: a variant plus its mandatory diagnosis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub ErrorDetail);

impl Error {
    pub fn new(
        kind: Kind,
        transport: impl Into<Option<Transport>>,
        component: Component,
        message: impl Into<String>,
    ) -> Self {
        Error(ErrorDetail {
            error: kind,
            transport: transport.into(),
            component,
            message: message.into(),
        })
    }

    pub const fn kind(&self) -> Kind {
        self.0.error
    }

    pub const fn component(&self) -> Component {
        self.0.component
    }

    pub const fn transport(&self) -> Option<Transport> {
        self.0.transport
    }

    pub fn message(&self) -> &str {
        &self.0.message
    }

    pub const fn exit_code(&self) -> i32 {
        self.0.error.exit_code()
    }

    /// The structured detail, ready to serialize into any of the three surfaces.
    pub const fn detail(&self) -> &ErrorDetail {
        &self.0
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.transport {
            Some(t) => write!(
                f,
                "{} [{t}/{}]: {}",
                self.0.error, self.0.component, self.0.message
            ),
            None => write!(
                f,
                "{} [{}]: {}",
                self.0.error, self.0.component, self.0.message
            ),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
