//! Wire protocol for the FIBER control socket (#79).
//!
//! The daemon (`fiber_app`) embeds the server (`super::server`); the `fiberctl`
//! binary is the client. They exchange newline-delimited JSON over a Unix
//! domain socket: the client writes exactly one [`Request`] line and reads
//! exactly one [`Response`] line, then the connection closes.
//!
//! The envelope is versioned (`v`) so client and daemon can evolve
//! independently; a server rejects a request whose `v` it does not support.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Bump on any breaking change to [`Command`] / [`Response`] semantics.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default control-socket path. Overridable via `FIBER_CONTROL_SOCKET`.
pub const DEFAULT_SOCKET_PATH: &str = "/run/fiber/control.sock";

/// Resolve the socket path: `FIBER_CONTROL_SOCKET` env override, else default.
pub fn socket_path() -> String {
    std::env::var("FIBER_CONTROL_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET_PATH.to_string())
}

/// One client→daemon request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Protocol version the client speaks ([`PROTOCOL_VERSION`]).
    pub v: u32,
    pub cmd: Command,
}

impl Request {
    pub fn new(cmd: Command) -> Self {
        Request { v: PROTOCOL_VERSION, cmd }
    }
}

/// The command set. Phase 1 (MVP): status + lorawan set/get/send + config.
/// Phase 2 will add ble/mqtt/sensors/actuators/power/storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Aggregate device status (version, power, network, lorawan, mqtt, sensors).
    Status,

    /// Write STICKER config via fPort-85 `SetParam` (#68). `fields` are
    /// `group.field` → string value (parsed + range-validated server-side).
    /// `save=true` commits + reboots the device.
    LorawanSetParam {
        dev_eui: String,
        fields: BTreeMap<String, String>,
        #[serde(default)]
        save: bool,
        /// Required for the destructive `save`/write path.
        #[serde(default)]
        force: bool,
    },

    /// Read back STICKER config via `GetParam` (#70) and decode it. If
    /// `desired` is set, also return the `diff_config` mismatches.
    LorawanGetParam {
        dev_eui: String,
        keys: Vec<String>,
        #[serde(default)]
        desired: Option<BTreeMap<String, String>>,
    },

    /// Send a no-argument fPort-85 command (#33/#34) and await the `Response`.
    LorawanSend {
        dev_eui: String,
        command: LorawanSimpleCommand,
        /// Required for destructive commands (reboot/factory_reset).
        #[serde(default)]
        force: bool,
    },

    /// Dump the effective application config.
    ConfigShow,
    /// Read a single config key (dotted path).
    ConfigGet { key: String },

    /// Current DS18B20 / line sensor readings.
    SensorsRead,
    /// Battery / DC power status.
    PowerStatus,
    /// MQTT broker connection state.
    MqttStatus,
    /// Apply a persistent config change (atomic write + reload). Destructive
    /// (mutates the on-disk config), so it requires `force`.
    ConfigSet {
        setting: ConfigSetting,
        #[serde(default)]
        force: bool,
    },
}

/// A settable config item, mapping 1:1 onto a `ConfigApplier::apply_*` method
/// (each does an atomic file write + backup + live reload).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigSetting {
    DeviceLabel { label: String },
    SensorName { line: u8, name: String },
    SensorLocation { line: u8, location: String },
    LedBrightness { value: u8 },
    ScreenBrightness { value: u8 },
    BuzzerVolume { value: u8 },
    SystemInfoInterval { seconds: u64 },
}

impl ConfigSetting {
    /// Short label for audit logging.
    pub fn audit_label(&self) -> &'static str {
        match self {
            ConfigSetting::DeviceLabel { .. } => "device_label",
            ConfigSetting::SensorName { .. } => "sensor_name",
            ConfigSetting::SensorLocation { .. } => "sensor_location",
            ConfigSetting::LedBrightness { .. } => "led_brightness",
            ConfigSetting::ScreenBrightness { .. } => "screen_brightness",
            ConfigSetting::BuzzerVolume { .. } => "buzzer_volume",
            ConfigSetting::SystemInfoInterval { .. } => "system_info_interval",
        }
    }

    /// Client-independent server-side range check (defense in depth, and honest
    /// `validation` errors regardless of whether the ConfigApplier validates).
    /// Returns the offending reason, or None if acceptable.
    pub fn validate(&self) -> Option<String> {
        match self {
            ConfigSetting::LedBrightness { value }
            | ConfigSetting::ScreenBrightness { value }
            | ConfigSetting::BuzzerVolume { value }
                if *value > 100 =>
            {
                Some(format!("{} must be 0-100, got {}", self.audit_label(), value))
            }
            ConfigSetting::SensorName { line, .. } | ConfigSetting::SensorLocation { line, .. }
                if *line > 7 =>
            {
                Some(format!("line must be 0-7, got {}", line))
            }
            _ => None,
        }
    }
}

/// No-argument fPort-85 commands exposed by `lorawan send`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LorawanSimpleCommand {
    GetInfo,
    Reboot,
    ForceSend,
    /// Reset all pulse counters (empty `ResetCounters` = all channels).
    ResetCounters,
    /// Push the daemon's current wall-clock to the device.
    ClockSync,
}

impl LorawanSimpleCommand {
    /// Commands that change device state / reboot it, so they require `--force`.
    pub fn is_destructive(self) -> bool {
        matches!(self, LorawanSimpleCommand::Reboot | LorawanSimpleCommand::ResetCounters)
    }
}

/// One daemon→client response. `data` is the command-specific payload (only
/// meaningful when `ok`); `error` carries a human-readable message otherwise.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default)]
    pub data: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Stable machine-readable error category (e.g. "not_enabled",
    /// "validation", "transport") so clients can branch on failure kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

impl Response {
    pub fn ok(data: serde_json::Value) -> Self {
        Response { ok: true, data, error: None, error_code: None }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Response { ok: false, data: serde_json::Value::Null, error: Some(msg.into()), error_code: None }
    }
    /// Error with a stable code + optional structured detail (e.g. validation list).
    pub fn err_coded(code: impl Into<String>, msg: impl Into<String>, data: serde_json::Value) -> Self {
        Response { ok: false, data, error: Some(msg.into()), error_code: Some(code.into()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_as_json_line() {
        let mut fields = BTreeMap::new();
        fields.insert("application.interval_report".to_string(), "600".to_string());
        let req = Request::new(Command::LorawanSetParam {
            dev_eui: "5876070000000001".into(),
            fields,
            save: true,
            force: true,
        });
        let line = serde_json::to_string(&req).unwrap();
        assert!(!line.contains('\n'));
        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(back.v, PROTOCOL_VERSION);
        match back.cmd {
            Command::LorawanSetParam { dev_eui, save, force, fields } => {
                assert_eq!(dev_eui, "5876070000000001");
                assert!(save && force);
                assert_eq!(fields["application.interval_report"], "600");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn simple_command_serde_is_snake_case() {
        let s = serde_json::to_string(&LorawanSimpleCommand::GetInfo).unwrap();
        assert_eq!(s, "\"get_info\"");
        assert!(LorawanSimpleCommand::Reboot.is_destructive());
        assert!(!LorawanSimpleCommand::GetInfo.is_destructive());
    }

    #[test]
    fn response_helpers() {
        let ok = Response::ok(serde_json::json!({"x": 1}));
        assert!(ok.ok && ok.error.is_none());
        let e = Response::err("nope");
        assert!(!e.ok && e.error.as_deref() == Some("nope"));
        // error omitted from JSON when None
        assert!(!serde_json::to_string(&ok).unwrap().contains("error"));
    }
}
