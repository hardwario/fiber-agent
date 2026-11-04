//! One error enum, surfaced identically through the CLI (exit code + `--json`), the harness and the
//! control socket — `plan/control/ble/plan.md` §6. The table is shared with `control/radio`: the same
//! number means the same thing in both dispatch groups (asserted in proximosctl's cross-crate test).

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Central,
    Peripheral,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::Central => "central",
            Role::Peripheral => "peripheral",
        }
    }
}
impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Closed vocabulary (decision #6 — declared surface). Extended only by adding a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Component {
    BluetoothAdapter,
    BluezDbus,
    GattServer,
    PhoneSession,
    BeaconGatt,
    BeaconRegistry,
    L2cap,
    Advertising,
    BleMode,
    MqttBroker,
    ControlSocket,
    DeviceConfig,
}
impl Component {
    pub const ALL: [Component; 12] = [
        Component::BluetoothAdapter,
        Component::BluezDbus,
        Component::GattServer,
        Component::PhoneSession,
        Component::BeaconGatt,
        Component::BeaconRegistry,
        Component::L2cap,
        Component::Advertising,
        Component::BleMode,
        Component::MqttBroker,
        Component::ControlSocket,
        Component::DeviceConfig,
    ];
    pub const fn as_str(self) -> &'static str {
        match self {
            Component::BluetoothAdapter => "bluetooth-adapter",
            Component::BluezDbus => "bluez-dbus",
            Component::GattServer => "gatt-server",
            Component::PhoneSession => "phone-session",
            Component::BeaconGatt => "beacon-gatt",
            Component::BeaconRegistry => "beacon-registry",
            Component::L2cap => "l2cap",
            Component::Advertising => "advertising",
            Component::BleMode => "ble-mode",
            Component::MqttBroker => "mqtt-broker",
            Component::ControlSocket => "control-socket",
            Component::DeviceConfig => "device-config",
        }
    }
}
impl fmt::Display for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The variant is the contract: 1:1 with an exit code, payload-free, wire name == Rust name here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    InvalidArgument,
    NotBootstrapped,
    NotFound,
    AlreadyExists,
    PairingRejected,
    ServiceDown,
    Timeout,
    Transport,
    DaemonDown,
    AdapterDown,
    BeaconUnreachable,
    Phase2NotImplemented,
    OffMode,
    CentralUnavailable,
    PeripheralUnavailable,
    RoleParameterRequired,
    CapabilityDisabled,
    StorageError,
}
impl Kind {
    pub const ALL: [Kind; 18] = [
        Kind::InvalidArgument,
        Kind::NotBootstrapped,
        Kind::NotFound,
        Kind::AlreadyExists,
        Kind::PairingRejected,
        Kind::ServiceDown,
        Kind::Timeout,
        Kind::Transport,
        Kind::DaemonDown,
        Kind::AdapterDown,
        Kind::BeaconUnreachable,
        Kind::Phase2NotImplemented,
        Kind::OffMode,
        Kind::CentralUnavailable,
        Kind::PeripheralUnavailable,
        Kind::RoleParameterRequired,
        Kind::CapabilityDisabled,
        Kind::StorageError,
    ];
    pub const fn as_str(self) -> &'static str {
        match self {
            Kind::InvalidArgument => "InvalidArgument",
            Kind::NotBootstrapped => "NotBootstrapped",
            Kind::NotFound => "NotFound",
            Kind::AlreadyExists => "AlreadyExists",
            Kind::PairingRejected => "PairingRejected",
            Kind::ServiceDown => "ServiceDown",
            Kind::Timeout => "Timeout",
            Kind::Transport => "Transport",
            Kind::DaemonDown => "DaemonDown",
            Kind::AdapterDown => "AdapterDown",
            Kind::BeaconUnreachable => "BeaconUnreachable",
            Kind::Phase2NotImplemented => "Phase2NotImplemented",
            Kind::OffMode => "OffMode",
            Kind::CentralUnavailable => "CentralUnavailable",
            Kind::PeripheralUnavailable => "PeripheralUnavailable",
            Kind::RoleParameterRequired => "RoleParameterRequired",
            Kind::CapabilityDisabled => "CapabilityDisabled",
            Kind::StorageError => "StorageError",
        }
    }
    /// Bands: 0 success (incl. degraded-green), 2 usage, 10–19 config facts (fail fast),
    /// 20–29 transient (retry with backoff), 30–39 mode/scope refusals (fail fast), 40+ storage.
    pub const fn exit_code(self) -> i32 {
        match self {
            Kind::InvalidArgument => 2,
            Kind::NotBootstrapped => 10,
            Kind::NotFound => 13,
            Kind::AlreadyExists => 14,
            Kind::PairingRejected => 15,
            Kind::ServiceDown => 20,
            Kind::Timeout => 21,
            Kind::Transport => 22,
            Kind::DaemonDown => 23,
            Kind::AdapterDown => 24,
            Kind::BeaconUnreachable => 25,
            Kind::Phase2NotImplemented => 34,
            Kind::OffMode => 35,
            Kind::CentralUnavailable => 36,
            Kind::PeripheralUnavailable => 37,
            Kind::RoleParameterRequired => 38,
            Kind::CapabilityDisabled => 39,
            Kind::StorageError => 40,
        }
    }
    /// True for the transient band. `ble-setup.service` retries these with backoff (bluetoothd may
    /// still be coming up behind `bt-address.service`) and fails fast on everything else.
    pub const fn is_retryable(self) -> bool {
        let c = self.exit_code();
        c >= 20 && c <= 29
    }
}
impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ErrorDetail {
    pub error: Kind,
    /// Which role produced it; `None` for a failure before dispatch picked one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    pub component: Component,
    /// Human-readable, for logs and the portal. **Never for branching.**
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub ErrorDetail);

impl Error {
    pub fn new(
        kind: Kind,
        role: impl Into<Option<Role>>,
        component: Component,
        message: impl Into<String>,
    ) -> Self {
        Error(ErrorDetail {
            error: kind,
            role: role.into(),
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
    pub const fn role(&self) -> Option<Role> {
        self.0.role
    }
    pub fn message(&self) -> &str {
        &self.0.message
    }
    pub const fn exit_code(&self) -> i32 {
        self.0.error.exit_code()
    }
    pub const fn detail(&self) -> &ErrorDetail {
        &self.0
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.role {
            Some(r) => write!(
                f,
                "{} [{r}/{}]: {}",
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
