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
        Request {
            v: PROTOCOL_VERSION,
            cmd,
        }
    }
}

/// The command set. Phase 1 (MVP): status + lorawan set/get/send + config.
/// Phase 2 will add ble/mqtt/sensors/actuators/power/storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Aggregate device status (version, power, network, lorawan, mqtt, sensors).
    Status,

    /// Write Node config via fPort-85 `SetParam` (#68). `fields` are
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

    /// Read back Node config via `GetParam` (#70) and decode it. If
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

    /// `proximosctl cluster client-add` — mint or rotate a Client Hub's broker account and
    /// register its gateway (plan/control/cluster.md §7). The same apply path as the signed
    /// `cluster-client-add`. The response's `data` is the result document on success and on a
    /// partial result alike. Its password is sealed to `reply_key`, exactly as on the signed
    /// path, and `proximosctl` opens it. `reply_key` and `request_id` are required. They are
    /// `Option` only so that a missing one is a `validation` answer that names it, rather than
    /// a parse error.
    ClusterClientAdd {
        hostname: String,
        #[serde(default)]
        gateway_eui: Option<String>,
        #[serde(default)]
        reply_key: Option<String>,
        #[serde(default)]
        request_id: Option<String>,
    },

    /// `proximosctl cluster client-remove` — revoke the account and deregister its gateway.
    ClusterClientRemove { hostname: String },

    /// `proximosctl control.radio create --radio lorawan --role client` — join this Hub to a
    /// Server as a Client (plan/control/cluster.md §8, the CLI twin of BLE FB1C), on the same
    /// apply path, the reboot included. The three fields are the join bundle's `h`, `p` and `f`;
    /// the password crosses this root-only socket and nothing else (`Secret` redacts `Debug`).
    ClusterJoin {
        server_host: String,
        password: crate::libs::cluster_join::Secret,
        ca_sha256: String,
    },

    /// Factory test (plan/system/factory_test.md): takes over the LCD, the
    /// buttons, the buzzer and the LEDs and starts the radio scans. Refused
    /// once the Hub is provisioned.
    FactoryTestStart,
    /// The test's state and the events after sequence number `since`.
    FactoryTestStatus {
        #[serde(default)]
        since: u64,
    },
    /// The operator saw the current step fail.
    FactoryTestFail {
        #[serde(default)]
        note: Option<String>,
    },
    /// Ends the test and hands the hardware back.
    FactoryTestStop,

    /// Reboot the Hub, as the Portal's `restart_application` does: audited,
    /// panel blanked, `systemctl reboot` (plan/system/factory_test.md §5.2).
    /// Refused without `force`.
    Reboot {
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        force: bool,
    },
    /// Put the Hub into deep standby, as the Portal's `power_off` does: dark,
    /// sensor rails down, back on when PoE returns or a front-panel button is
    /// held. Not `systemctl poweroff`, which only the back button undoes (see
    /// [`Command::Shutdown`]). Refused without `force`.
    Standby {
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        force: bool,
    },
    /// Shut the Hub down for good: audited, panel blanked, `systemctl poweroff`.
    /// Only the button on the back (S1, the 5 V rail's enable) brings it back;
    /// PoE does not (plan/system/factory_test.md §5.2). Refused without `force`.
    Shutdown {
        #[serde(default)]
        reason: Option<String>,
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
                Some(format!(
                    "{} must be 0-100, got {}",
                    self.audit_label(),
                    value
                ))
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
    /// Reset all four resettable pulse counters. Each flag is sent explicitly:
    /// the firmware treats an absent flag as "leave this counter alone", so an
    /// empty message would Ack without resetting anything.
    ResetCounters,
    /// Push the daemon's current wall-clock to the device.
    ClockSync,
    /// Restore defaults, keeping identity and the LoRaWAN keys (proto id 8). The
    /// node stays joined; all parameters and alarm rules are lost.
    DeviceReset,
    /// The NFC/shell-only factory reset (proto id 23). **Always rejected over
    /// LoRaWAN** with `Error{NOT_READY, "transport not allowed"}` — exposed so the
    /// rejection can be demonstrated on the bench, not to be used operationally.
    FactoryReset,
}

impl LorawanSimpleCommand {
    /// Commands that change device state / reboot it, so they require `--force`.
    pub fn is_destructive(self) -> bool {
        matches!(
            self,
            LorawanSimpleCommand::Reboot
                | LorawanSimpleCommand::ResetCounters
                | LorawanSimpleCommand::DeviceReset
                | LorawanSimpleCommand::FactoryReset
        )
    }

    /// Commands that leave a deferred action pending on the device's single
    /// `m_post_cmd_action` slot, so they must not overlap on one node.
    pub fn is_action_bearing(self) -> bool {
        matches!(
            self,
            LorawanSimpleCommand::Reboot
                | LorawanSimpleCommand::ResetCounters
                | LorawanSimpleCommand::DeviceReset
        )
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
        Response {
            ok: true,
            data,
            error: None,
            error_code: None,
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Response {
            ok: false,
            data: serde_json::Value::Null,
            error: Some(msg.into()),
            error_code: None,
        }
    }
    /// Error with a stable code + optional structured detail (e.g. validation list).
    pub fn err_coded(
        code: impl Into<String>,
        msg: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        Response {
            ok: false,
            data,
            error: Some(msg.into()),
            error_code: Some(code.into()),
        }
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
            Command::LorawanSetParam {
                dev_eui,
                save,
                force,
                fields,
            } => {
                assert_eq!(dev_eui, "5876070000000001");
                assert!(save && force);
                assert_eq!(fields["application.interval_report"], "600");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// The exact lines `proximosctl cluster` writes (`proximosctl/src/cluster.rs` pins the same
    /// strings from its side; neither crate can depend on the other's types).
    #[test]
    fn the_cluster_requests_proximosctl_sends_parse() {
        let add: Request = serde_json::from_str(
            r#"{"v":1,"cmd":{"type":"cluster_client_add","hostname":"proximos-5e6f7a8b","gateway_eui":"0016c001ff10a235","reply_key":"1wWpoC1yfr-Mw9EeK1ML134Q3CxHu5Otu_l1wzNrSy0","request_id":"cli-0123456789abcdef"}}"#,
        )
        .unwrap();
        match add.cmd {
            Command::ClusterClientAdd {
                hostname,
                gateway_eui,
                reply_key,
                request_id,
            } => {
                assert_eq!(hostname, "proximos-5e6f7a8b");
                assert_eq!(gateway_eui.as_deref(), Some("0016c001ff10a235"));
                assert_eq!(
                    reply_key.as_deref(),
                    Some("1wWpoC1yfr-Mw9EeK1ML134Q3CxHu5Otu_l1wzNrSy0")
                );
                assert_eq!(request_id.as_deref(), Some("cli-0123456789abcdef"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        let bare: Request = serde_json::from_str(
            r#"{"v":1,"cmd":{"type":"cluster_client_add","hostname":"proximos-5e6f7a8b","reply_key":"1wWpoC1yfr-Mw9EeK1ML134Q3CxHu5Otu_l1wzNrSy0","request_id":"cli-0123456789abcdef"}}"#,
        )
        .unwrap();
        assert!(matches!(
            bare.cmd,
            Command::ClusterClientAdd {
                gateway_eui: None,
                ..
            }
        ));
        let remove: Request = serde_json::from_str(
            r#"{"v":1,"cmd":{"type":"cluster_client_remove","hostname":"proximos-5e6f7a8b"}}"#,
        )
        .unwrap();
        assert!(matches!(remove.cmd, Command::ClusterClientRemove { .. }));
    }

    /// The line `proximosctl control.radio create --role client` writes (control-radio's
    /// `cluster_join::tests::the_request_is_the_one_the_agent_parses` pins the same string from
    /// its side).
    #[test]
    fn the_cluster_join_request_proximosctl_sends_parses() {
        let join: Request = serde_json::from_str(
            r#"{"v":1,"cmd":{"type":"cluster_join","server_host":"proximos-1a2b3c4d.local","password":"Zm9vYmFyYmF6cXV4cXV1eA","ca_sha256":"x"}}"#,
        )
        .unwrap();
        match &join.cmd {
            Command::ClusterJoin {
                server_host,
                password,
                ca_sha256,
            } => {
                assert_eq!(server_host, "proximos-1a2b3c4d.local");
                assert_eq!(password.expose(), "Zm9vYmFyYmF6cXV4cXV1eA");
                assert_eq!(ca_sha256, "x");
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(
            !format!("{join:?}").contains("Zm9vYmFyYmF6cXV4cXV1eA"),
            "a logged request never shows the password"
        );
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
