// MQTT message types for channel communication

use crate::libs::alarms::{AlarmState, AlarmThreshold};
use crate::libs::crypto::UserCertificate;
use crate::libs::pairing::messages::{PairingError, PairingResponse};
use crate::libs::sensors::aggregation::AggregationPeriod;
use serde_json::Value;
use std::collections::BTreeMap;

/// Messages sent to the MQTT monitor thread for publishing
#[derive(Debug, Clone)]
pub enum MqttMessage {
    /// Publish aggregated sensor data
    PublishAggregatedSensorData {
        period: AggregationPeriod,
        names: [String; 8],
        locations: [Option<String>; 8],
    },

    /// Publish an alarm state transition event
    PublishAlarmEvent {
        line: u8,
        name: String,
        from_state: AlarmState,
        to_state: AlarmState,
        temperature: f32,
    },

    /// Publish a system-level alarm event (power, wifi, ethernet)
    PublishSystemAlarmEvent {
        alarm_type: String,     // "POWER_DISCONNECT", "WIFI_DISCONNECT", "ETHERNET_DISCONNECT"
        name: String,           // "Power Supply", "WiFi", "Ethernet"
        from_state: String,     // "NORMAL" or "CRITICAL"
        to_state: String,       // "CRITICAL" or "NORMAL"
        message: String,        // Human-readable message
    },

    /// Publish an accelerometer motion transition event
    PublishAccelerometerEvent {
        x_g: f32,       // X-axis acceleration at transition (g)
        y_g: f32,       // Y-axis acceleration at transition (g)
        z_g: f32,       // Z-axis acceleration at transition (g)
        position: u8,   // Box orientation 1..6 (see MotionDetector::position)
    },

    /// Publish combined system status (power, network, storage, uptime)
    PublishSystemStatus {
        /// Hostname
        hostname: String,
        /// Device label (user-friendly name)
        device_label: String,
        /// Firmware version
        version: String,
        /// Uptime in seconds
        uptime_seconds: u64,
        /// Battery voltage in mV
        battery_mv: u16,
        /// Battery percentage (0-100)
        battery_percent: u8,
        /// Input voltage in mV
        vin_mv: u16,
        /// On DC power
        on_dc_power: bool,
        /// Last DC loss timestamp (epoch seconds)
        last_dc_loss_time: Option<u64>,
        /// WiFi connected
        wifi_connected: bool,
        /// WiFi signal in dBm
        wifi_signal_dbm: i32,
        /// WiFi IP address
        wifi_ip: Option<String>,
        /// Ethernet connected
        ethernet_connected: bool,
        /// Ethernet IP address
        ethernet_ip: Option<String>,
        /// Storage total bytes
        storage_total_bytes: u64,
        /// Storage available bytes
        storage_available_bytes: u64,
        /// Storage used percent
        storage_used_percent: u8,
        /// LoRaWAN gateway present
        lorawan_gateway_present: bool,
        /// LoRaWAN concentratord running
        lorawan_concentratord_running: bool,
        /// LoRaWAN chirpstack running
        lorawan_chirpstack_running: bool,
        /// LoRaWAN sensor count
        lorawan_sensor_count: usize,
    },

    /// Publish configuration challenge (preview of changes)
    PublishConfigChallenge {
        challenge_id: String,
        request_id: String,
        signer_id: String,
        expires_at: i64,
        preview: Value, // ChangePreview as JSON
    },

    /// Publish configuration response (success/error)
    PublishConfigResponse {
        challenge_id: String,
        request_id: String,
        status: String, // SUCCESS, ERROR
        applied_at: Option<i64>,
        effective_at: Option<i64>,
        message: String,
    },

    /// Publish sensor configuration data
    PublishSensorConfig {
        sensors: Vec<SensorConfigData>,
    },

    /// Publish interval configuration data
    PublishIntervalConfig {
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        report_interval_ms: u64,
    },

    /// Publish full device config state (brightness, intervals, sensors, label)
    PublishConfigState {
        led_brightness: u8,
        screen_brightness: u8,
        buzzer_volume: u8,
        system_info_interval_s: u64,
        device_label: String,
        sensors: Vec<SensorConfigData>,
        lorawan_sensors: Vec<LoRaWANSensorConfigData>,
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        report_interval_ms: u64,
    },

    /// Publish LoRaWAN sensor data
    PublishLoRaWANSensorData {
        sensors: Vec<LoRaWANSensorPayload>,
    },

    /// Publish external LoRaWAN gateway status
    PublishLoRaWANGatewayData {
        gateways: Vec<LoRaWANGatewayPayload>,
    },

    /// Publish a STICKER's fPort-85 config read-back (Feature C) to
    /// `lorawan/sensors/<dev_eui>/config`.
    PublishStickerConfig {
        dev_eui: String,
        /// Flattened `group.field` → JSON value (projected from ConfigValue).
        config: BTreeMap<String, Value>,
        page_index: u32,
        page_count: u32,
        /// Seq + result of the last Ack/Error (the viewer renders
        /// pending / awaiting-Ack / ok from this).
        last_seq: u32,
        last_result: String,
    },

    /// Publish one page of a STICKER's on-device history (Feature D) to
    /// `lorawan/sensors/<dev_eui>/history`.
    PublishStickerHistory {
        dev_eui: String,
        frame_index: u32,
        frame_count: u32,
        /// Each record is `{ time, fields{}, counters{} }` as JSON.
        records: Vec<Value>,
    },

    /// Publish EYE BLE tag sensor data
    PublishEyeSensorData {
        tags: Vec<EyeTagPayload>,
    },

    /// Publish successful pairing response
    PublishPairingResponse(PairingResponse),

    /// Publish pairing error
    PublishPairingError(PairingError),

    /// Update connection state (internal message)
    SetConnectionState(super::connection::ConnectionState),

    /// Graceful shutdown signal
    Shutdown,
}

/// External LoRaWAN gateway status payload for MQTT publishing.
#[derive(Debug, Clone)]
pub struct LoRaWANGatewayPayload {
    pub gateway_eui: String,
    pub name: Option<String>,
    pub online: bool,
    pub last_seen: Option<String>,
}

/// EYE BLE tag data payload for MQTT publishing.
#[derive(Debug, Clone)]
pub struct EyeTagPayload {
    pub mac: String,
    pub name: Option<String>,
    pub temperature_c: Option<f32>,
    pub humidity_pct: Option<u8>,
    pub battery_mv: Option<u16>,
    pub low_battery: bool,
    pub magnet_present: bool,
    pub magnet_detected: bool,
    pub moving: Option<bool>,
    pub movement_count: Option<u16>,
    pub pitch_deg: Option<i8>,
    pub roll_deg: Option<i16>,
    pub rssi: Option<i16>,
    pub last_seen_ts: Option<i64>,
    /// Whether the tag has not been seen within the configured `tag_timeout_s`.
    pub stale: bool,
    pub provisioning: String,
}

/// LoRaWAN sensor data payload for MQTT publishing (v2 generic-field model)
#[derive(Debug, Clone)]
pub struct LoRaWANSensorPayload {
    pub dev_eui: String,
    pub name: String,
    pub serial_number: Option<String>,
    pub location: Option<String>,
    pub fields: std::collections::HashMap<String, f64>,
    pub field_alarm_states: std::collections::HashMap<String, String>,
    pub field_thresholds: Vec<crate::libs::config::FieldThreshold>,
    pub counters: std::collections::HashMap<String, u64>,
    pub events: Vec<crate::libs::lorawan::chirpstack::StickerEvent>,
    pub rssi: Option<i32>,
    pub snr: Option<f32>,
    pub last_seen: Option<String>,
    pub alarm_state: String,
}

/// Sensor configuration data for query response
#[derive(Debug, Clone)]
pub struct SensorConfigData {
    pub line: u8,
    pub name: String,
    pub location: Option<String>,
    pub enabled: bool,
    pub has_override: bool, // true if using per-line thresholds, false if using common defaults
    pub thresholds: AlarmThreshold,
}

/// LoRaWAN sensor configuration data for config state publishing (v2)
#[derive(Debug, Clone)]
pub struct LoRaWANSensorConfigData {
    pub dev_eui: String,
    pub name: Option<String>,
    pub serial_number: Option<String>,
    pub location: Option<String>,
    pub enabled: bool,
    pub field_thresholds: Vec<crate::libs::config::FieldThreshold>,
}

fn default_join_eui() -> String {
    "0000000000000000".to_string()
}

/// LoRaWAN activation mode for STICKER registration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum ActivationMode {
    /// OTAA: device joins the network using AppKey + JoinEUI.
    Otaa {
        app_key: String,
        /// 16 hex chars. Defaults to all-zeros for compatibility with viewers
        /// that pre-date the configurable JoinEUI field.
        #[serde(default = "default_join_eui")]
        join_eui: String,
    },
    /// ABP: device pre-personalised with session keys.
    Abp {
        devaddr: String,
        nwkskey: String,
        appskey: String,
    },
}

/// Commands received from MQTT broker
#[derive(Debug, Clone)]
pub enum MqttCommand {
    /// Set sensor alarm threshold
    SetSensorThreshold {
        line: u8,
        critical_low: f32,
        alarm_low: f32,
        warning_low: f32,
        warning_high: f32,
        alarm_high: f32,
        critical_high: f32,
    },

    /// Get current sensor status
    GetSensorStatus { line: u8 },

    /// Switch display screen
    SetDisplayScreen { screen: String },

    /// Flush storage to disk
    FlushStorage,

    /// Get device information
    GetDeviceInfo,

    /// Get sensor configuration (all 8 sensors)
    GetSensorConfig,

    /// Set sensor name (signed via ConfigRequest)
    SetSensorName { line: u8, name: String },

    /// Set sensor probe location (signed via ConfigRequest)
    SetSensorLocation { line: u8, location: String },

    /// Restart application
    RestartApplication { reason: String },

    /// Set sensor intervals (sample, aggregation, report)
    SetInterval {
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        report_interval_ms: u64,
    },

    /// Get current sensor intervals
    GetInterval,

    /// Set system info report interval (signed via ConfigRequest)
    SetSystemInfoInterval {
        interval_seconds: u64,
    },

    /// Set device label (signed via ConfigRequest)
    SetDeviceLabel {
        label: String,
    },

    /// Set LED brightness (signed via ConfigRequest)
    SetLedBrightness {
        brightness: u8,
    },

    /// Set screen brightness (signed via ConfigRequest)
    SetScreenBrightness {
        brightness: u8,
    },

    /// Set buzzer volume (signed via ConfigRequest)
    /// 0 = muted, 1-100 = active (full volume)
    SetBuzzerVolume {
        volume: u8,
    },

    /// Silence buzzer (from alarm acknowledgment)
    /// Stops current pattern but re-arms for new alarms
    SilenceBuzzer,

    /// Set network configuration (signed via ConfigRequest)
    SetNetworkConfig {
        interface: String,      // "ethernet" or "wifi"
        config_type: String,    // "dhcp" or "static"
        ip_address: Option<String>,
        subnet_mask: Option<String>,
        gateway: Option<String>,
        dns_primary: Option<String>,
        dns_secondary: Option<String>,
    },

    /// Set LoRaWAN sensor metadata (name/serial/location) — signed via ConfigRequest.
    /// Per-field thresholds live in dedicated commands (`SetLoRaWANFieldThreshold` / `DeleteLoRaWANFieldThreshold`).
    SetLoRaWANSensorConfig {
        dev_eui: String,
        name: Option<String>,
        serial_number: Option<String>,
        location: Option<String>,
    },

    /// Set a single per-field threshold for a LoRaWAN sensor
    SetLoRaWANFieldThreshold {
        dev_eui: String,
        field: String,
        critical_low: Option<f64>,
        warning_low: Option<f64>,
        warning_high: Option<f64>,
        critical_high: Option<f64>,
    },

    /// Remove a per-field threshold for a LoRaWAN sensor
    DeleteLoRaWANFieldThreshold {
        dev_eui: String,
        field: String,
    },

    /// Add LoRaWAN sticker: provision in ChirpStack + save sensor config (signed via ConfigRequest)
    AddLoRaWANSticker {
        dev_eui: String,
        name: String,
        serial_number: String,
        activation: ActivationMode,
    },

    /// Remove LoRaWAN sticker: remove sensor config (signed via ConfigRequest)
    RemoveLoRaWANSticker {
        dev_eui: String,
    },

    /// Read a STICKER's own fPort-85 parameters over MQTT (unsigned query).
    /// `keys` empty/None = read the full settable set.
    GetStickerConfig {
        dev_eui: String,
        keys: Option<Vec<String>>,
    },

    /// Write a STICKER's fPort-85 parameters over MQTT (signed via ConfigRequest).
    /// `fields` maps SETTABLE keys (`application.interval_report`, …) to string
    /// values parsed + range-checked by the fPort-85 engine. `save` persists to
    /// flash (reboots the sticker) vs RAM-staging a dry run.
    SetStickerConfig {
        dev_eui: String,
        fields: BTreeMap<String, String>,
        save: bool,
    },

    /// Request a STICKER's on-device history buffer (fPort-85 ReqHistory,
    /// unsigned query). `from_unix`/`to_unix` bound the window; None = whole buffer.
    GetStickerHistory {
        dev_eui: String,
        from_unix: Option<u32>,
        to_unix: Option<u32>,
    },

    /// Send a raw fPort downlink to a STICKER verbatim (signed via ConfigRequest).
    /// `bytes` is the exact protobuf `Command` produced by an operator's downlink
    /// generator; `fport` defaults to 85. Fire-and-forget — no response is
    /// correlated (expert / advanced use).
    SendStickerRaw {
        dev_eui: String,
        bytes: Vec<u8>,
        fport: u8,
    },

    /// Set the EN12830 recording interval for an EYE tag and (re)start recording.
    SetEyeRecording {
        mac: String,
        interval_min: u16,
    },

    /// Manually back-fill the EN12830 temperature archive for an EYE tag.
    DownloadEyeHistory {
        mac: String,
    },

    /// Reset the save-and-feed export cursor for `(broker_id, stream)` so the
    /// next drain pass replays the stream from row 1. Use after a viewer DB
    /// wipe or to force a backfill. `stream` may be "sticker" | "probe" |
    /// "alarm" | "all".
    ResetExportCursor {
        broker_id: String,
        stream: String,
    },

    /// On-demand replay of `sensor_readings_minute` for a historical window.
    /// Triggered by the viewer when the user navigates past the 30-day hot
    /// tier: the device replays the requested range on the
    /// `export/probe_1m_replay/<request_id>/<sensor_line>` topic without
    /// touching the natural drain cursor.
    HistoryRequest {
        request_id: String,
        /// `None` means "all sensor lines 0..=7".
        sensor_line: Option<u8>,
        from_ts: i64,
        to_ts: i64,
    },

    /// Add signer (signed via ConfigRequest)
    AddSigner { signer_data: Value },

    /// Remove signer (signed via ConfigRequest)
    RemoveSigner { signer_id: String },

    /// Update signer (signed via ConfigRequest)
    UpdateSigner {
        signer_id: String,
        changes: Value,
    },

    /// Configuration change request (signed with Ed25519)
    ConfigRequest {
        request_id: String,
        command_type: String,
        params: Value, // Command-specific parameters as JSON
        reason: Option<String>,
        signer_id: String,
        signature: String, // Base64-encoded Ed25519 signature
        timestamp: i64,
        nonce: String,
        certificate: UserCertificate, // User certificate signed by CA
    },

    /// Configuration change confirmation (signed)
    ConfigConfirm {
        challenge_id: String,
        confirmation: String, // APPROVED or REJECTED
        signer_id: String,
        signature: String, // Base64-encoded Ed25519 signature
        timestamp: i64,
        nonce: String,
        certificate: UserCertificate, // User certificate signed by CA
    },

    /// Pairing request from viewer backend
    PairingRequest {
        request_id: String,
        timestamp: i64,
        admin_username: String,
    },
}

impl MqttCommand {
    /// Get command name for logging
    pub fn name(&self) -> &'static str {
        match self {
            MqttCommand::SetSensorThreshold { .. } => "set_sensor_threshold",
            MqttCommand::GetSensorStatus { .. } => "get_sensor_status",
            MqttCommand::SetDisplayScreen { .. } => "set_display_screen",
            MqttCommand::FlushStorage => "flush_storage",
            MqttCommand::GetDeviceInfo => "get_device_info",
            MqttCommand::GetSensorConfig => "get_sensor_config",
            MqttCommand::SetSensorName { .. } => "set_sensor_name",
            MqttCommand::SetSensorLocation { .. } => "set_sensor_location",
            MqttCommand::RestartApplication { .. } => "restart_application",
            MqttCommand::SetInterval { .. } => "set_interval",
            MqttCommand::GetInterval => "get_interval",
            MqttCommand::SetSystemInfoInterval { .. } => "set_system_info_interval",
            MqttCommand::SetDeviceLabel { .. } => "set_device_label",
            MqttCommand::SetLedBrightness { .. } => "set_led_brightness",
            MqttCommand::SetScreenBrightness { .. } => "set_screen_brightness",
            MqttCommand::SetBuzzerVolume { .. } => "set_buzzer_volume",
            MqttCommand::SilenceBuzzer => "silence_buzzer",
            MqttCommand::SetNetworkConfig { .. } => "set_network_config",
            MqttCommand::SetLoRaWANSensorConfig { .. } => "set_lorawan_sensor_config",
            MqttCommand::SetLoRaWANFieldThreshold { .. } => "set_lorawan_field_threshold",
            MqttCommand::DeleteLoRaWANFieldThreshold { .. } => "delete_lorawan_field_threshold",
            MqttCommand::AddLoRaWANSticker { .. } => "add_lorawan_sticker",
            MqttCommand::SetEyeRecording { .. } => "set_eye_recording",
            MqttCommand::DownloadEyeHistory { .. } => "download_eye_history",
            MqttCommand::RemoveLoRaWANSticker { .. } => "remove_lorawan_sticker",
            MqttCommand::GetStickerConfig { .. } => "get_sticker_config",
            MqttCommand::SetStickerConfig { .. } => "set_sticker_config",
            MqttCommand::SendStickerRaw { .. } => "send_sticker_raw",
            MqttCommand::GetStickerHistory { .. } => "get_sticker_history",
            MqttCommand::ResetExportCursor { .. } => "reset_export_cursor",
            MqttCommand::HistoryRequest { .. } => "history_request",
            MqttCommand::AddSigner { .. } => "add_signer",
            MqttCommand::RemoveSigner { .. } => "remove_signer",
            MqttCommand::UpdateSigner { .. } => "update_signer",
            MqttCommand::ConfigRequest { .. } => "config_request",
            MqttCommand::ConfigConfirm { .. } => "config_confirm",
            MqttCommand::PairingRequest { .. } => "pairing_request",
        }
    }

    /// Parse a `set_sticker_config` params object `{dev_eui, config{}, save?}`
    /// into [`MqttCommand::SetStickerConfig`]. Shared by the production
    /// (challenge) and dev-platform signed-command builders so both paths parse
    /// identically. Config values may be JSON string/number/bool and are
    /// stringified for the fPort-85 engine's typed parser.
    pub fn parse_set_sticker_config(params: &Value) -> Result<MqttCommand, String> {
        let dev_eui = params
            .get("dev_eui")
            .and_then(|v| v.as_str())
            .ok_or("Missing dev_eui")?;
        if dev_eui.len() != 16 || !dev_eui.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("Invalid dev_eui {:?} (expected 16 hex chars)", dev_eui));
        }
        let config_obj = params
            .get("config")
            .and_then(|v| v.as_object())
            .ok_or("Missing or invalid 'config' object")?;
        let mut fields = BTreeMap::new();
        for (k, v) in config_obj {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                _ => {
                    return Err(format!(
                        "config value for '{}' must be a string, number or bool",
                        k
                    ))
                }
            };
            fields.insert(k.clone(), s);
        }
        if fields.is_empty() {
            return Err("config must contain at least one key".to_string());
        }
        let save = params.get("save").and_then(|v| v.as_bool()).unwrap_or(false);
        Ok(MqttCommand::SetStickerConfig {
            dev_eui: dev_eui.to_lowercase(),
            fields,
            save,
        })
    }

    /// Parse a `send_sticker_raw` params object `{dev_eui, hex, fport?}` into
    /// [`MqttCommand::SendStickerRaw`]. `hex` is an even-length hex string of at
    /// most 51 bytes (the DR0 downlink budget); `fport` defaults to 85 (1..=223).
    pub fn parse_send_sticker_raw(params: &Value) -> Result<MqttCommand, String> {
        let dev_eui = params
            .get("dev_eui")
            .and_then(|v| v.as_str())
            .ok_or("Missing dev_eui")?;
        if dev_eui.len() != 16 || !dev_eui.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("Invalid dev_eui {:?} (expected 16 hex chars)", dev_eui));
        }
        let hex_str = params
            .get("hex")
            .and_then(|v| v.as_str())
            .ok_or("Missing hex")?
            .trim();
        let bytes = hex::decode(hex_str).map_err(|e| format!("Invalid hex: {e}"))?;
        if bytes.is_empty() {
            return Err("hex must contain at least one byte".to_string());
        }
        if bytes.len() > 51 {
            return Err(format!("raw downlink too large: {} bytes (max 51)", bytes.len()));
        }
        let fport = match params.get("fport") {
            None => 85u8,
            Some(v) => {
                let n = v.as_u64().ok_or("fport must be a number")?;
                if !(1..=223).contains(&n) {
                    return Err(format!("fport {} out of range (1..=223)", n));
                }
                n as u8
            }
        };
        Ok(MqttCommand::SendStickerRaw {
            dev_eui: dev_eui.to_lowercase(),
            bytes,
            fport,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_names() {
        let cmd = MqttCommand::SetSensorThreshold {
            line: 0,
            critical_low: 32.0,
            alarm_low: 34.0,
            warning_low: 35.0,
            warning_high: 39.0,
            alarm_high: 40.0,
            critical_high: 42.0,
        };

        assert_eq!(cmd.name(), "set_sensor_threshold");

        let cmd2 = MqttCommand::FlushStorage;
        assert_eq!(cmd2.name(), "flush_storage");
    }

    #[test]
    fn reset_export_cursor_command_has_name_and_carries_fields() {
        // MqttCommand isn't serde-derived (other variants carry non-serde
        // types), so we exercise the variant directly rather than via JSON
        // roundtrip. Parsing from JSON is the subscriber's job.
        let cmd = MqttCommand::ResetExportCursor {
            broker_id: "local".into(),
            stream: "all".into(),
        };
        assert_eq!(cmd.name(), "reset_export_cursor");
        match cmd {
            MqttCommand::ResetExportCursor { broker_id, stream } => {
                assert_eq!(broker_id, "local");
                assert_eq!(stream, "all");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn sticker_commands_have_names() {
        assert_eq!(
            MqttCommand::GetStickerConfig { dev_eui: "0102030405060708".into(), keys: None }.name(),
            "get_sticker_config"
        );
        let mut fields = BTreeMap::new();
        fields.insert("application.interval_report".to_string(), "1200".to_string());
        assert_eq!(
            MqttCommand::SetStickerConfig {
                dev_eui: "0102030405060708".into(),
                fields,
                save: false,
            }
            .name(),
            "set_sticker_config"
        );
        assert_eq!(
            MqttCommand::GetStickerHistory {
                dev_eui: "0102030405060708".into(),
                from_unix: None,
                to_unix: None,
            }
            .name(),
            "get_sticker_history"
        );
    }

    #[test]
    fn parse_send_sticker_raw_validates_and_defaults() {
        use serde_json::json;
        assert_eq!(
            MqttCommand::SendStickerRaw { dev_eui: "0102030405060708".into(), bytes: vec![8], fport: 85 }.name(),
            "send_sticker_raw"
        );
        // The docs.hardwario.com generator example (SetParam interval_report=600, save).
        let cmd = MqttCommand::parse_send_sticker_raw(
            &json!({ "dev_eui": "d7653371A0EF363F", "hex": "08011207120318d8041801" }),
        )
        .unwrap();
        match cmd {
            MqttCommand::SendStickerRaw { dev_eui, bytes, fport } => {
                assert_eq!(dev_eui, "d7653371a0ef363f"); // lowercased
                assert_eq!(fport, 85); // default
                assert_eq!(bytes, vec![0x08, 0x01, 0x12, 0x07, 0x12, 0x03, 0x18, 0xd8, 0x04, 0x18, 0x01]);
            }
            _ => panic!("wrong variant"),
        }
        // fport override in range.
        assert!(matches!(
            MqttCommand::parse_send_sticker_raw(&json!({ "dev_eui": "0102030405060708", "hex": "08", "fport": 10 })).unwrap(),
            MqttCommand::SendStickerRaw { fport: 10, .. }
        ));
        // Rejections: bad hex, bad dev_eui, empty, oversize (52 bytes), fport OOR.
        assert!(MqttCommand::parse_send_sticker_raw(&json!({ "dev_eui": "0102030405060708", "hex": "zz" })).is_err());
        assert!(MqttCommand::parse_send_sticker_raw(&json!({ "dev_eui": "short", "hex": "08" })).is_err());
        assert!(MqttCommand::parse_send_sticker_raw(&json!({ "dev_eui": "0102030405060708", "hex": "" })).is_err());
        assert!(MqttCommand::parse_send_sticker_raw(&json!({ "dev_eui": "0102030405060708", "hex": "aa".repeat(52) })).is_err());
        assert!(MqttCommand::parse_send_sticker_raw(&json!({ "dev_eui": "0102030405060708", "hex": "08", "fport": 300 })).is_err());
    }

    #[test]
    fn activation_mode_otaa_roundtrip() {
        let app_key: String = "ab".repeat(16);
        let join_eui: String = "cd".repeat(8);
        let v = ActivationMode::Otaa {
            app_key: app_key.clone(),
            join_eui: join_eui.clone(),
        };
        let s = serde_json::to_value(&v).unwrap();
        assert_eq!(
            s,
            serde_json::json!({"mode": "otaa", "app_key": app_key, "join_eui": join_eui})
        );
        let back: ActivationMode = serde_json::from_value(s).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn activation_mode_otaa_legacy_payload_defaults_join_eui_to_zeros() {
        // Old viewers send {"mode": "otaa", "app_key": "..."} with no join_eui.
        let app_key: String = "ab".repeat(16);
        let payload = serde_json::json!({"mode": "otaa", "app_key": app_key});
        let back: ActivationMode = serde_json::from_value(payload).unwrap();
        match back {
            ActivationMode::Otaa { join_eui, .. } => {
                assert_eq!(join_eui, "0000000000000000");
            }
            _ => panic!("expected Otaa"),
        }
    }

    #[test]
    fn activation_mode_abp_roundtrip() {
        let nwkskey: String = "0".repeat(32);
        let appskey: String = "f".repeat(32);
        let v = ActivationMode::Abp {
            devaddr: "01020304".to_string(),
            nwkskey: nwkskey.clone(),
            appskey: appskey.clone(),
        };
        let s = serde_json::to_value(&v).unwrap();
        assert_eq!(s, serde_json::json!({
            "mode": "abp",
            "devaddr": "01020304",
            "nwkskey": nwkskey,
            "appskey": appskey,
        }));
        let back: ActivationMode = serde_json::from_value(s).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn activation_mode_unknown_mode_rejected() {
        let s = serde_json::json!({"mode": "xyz"});
        let r: Result<ActivationMode, _> = serde_json::from_value(s);
        assert!(r.is_err());
    }
}
