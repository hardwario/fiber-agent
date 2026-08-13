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
        alarm_type: String, // "POWER_DISCONNECT", "WIFI_DISCONNECT", "ETHERNET_DISCONNECT"
        name: String,       // "Power Supply", "WiFi", "Ethernet"
        from_state: String, // "NORMAL" or "CRITICAL"
        to_state: String,   // "CRITICAL" or "NORMAL"
        message: String,    // Human-readable message
    },

    /// Publish the device's standby state.
    ///
    /// Retained, because "this device is off because someone turned it off" is
    /// only useful if a Viewer that subscribes later still learns it. Without a
    /// retained state a standby device is indistinguishable from a crashed one.
    PublishStandbyState {
        standby: bool,
        /// Reason from the signed power-off command, or how it woke.
        reason: String,
        /// Signer of the power-off. Empty when the device woke by itself.
        requested_by: String,
        /// RFC 3339, when standby began. `None` once awake.
        entered_at: Option<String>,
        vin_mv: u16,
    },

    /// Publish an accelerometer motion transition event
    PublishAccelerometerEvent {
        x_g: f32,     // X-axis acceleration at transition (g)
        y_g: f32,     // Y-axis acceleration at transition (g)
        z_g: f32,     // Z-axis acceleration at transition (g)
        position: u8, // Box orientation 1..6 (see MotionDetector::position)
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
    PublishSensorConfig { sensors: Vec<SensorConfigData> },

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
        screen_timeout_secs: u32,
        buzzer_volume: u8,
        system_info_interval_s: u64,
        device_label: String,
        sensors: Vec<SensorConfigData>,
        lorawan_sensors: Vec<LoRaWANSensorConfigData>,
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        report_interval_ms: u64,
        /// EYE subsystem flags, so the viewer can reflect the real state of the
        /// auto-provision / auto-discover toggles.
        eye_enabled: bool,
        eye_auto_provision: bool,
        eye_auto_discover: bool,
    },

    /// Publish LoRaWAN sensor data
    PublishLoRaWANSensorData { sensors: Vec<LoRaWANSensorPayload> },

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

    /// Publish a STICKER's full non-secret config read-back to
    /// `lorawan/sensors/<dev_eui>/full-config`.
    ///
    /// A wide read is many Class-A round trips, so a chunk failing is normal
    /// rather than exceptional: `read_status` and `missing` say which keys never
    /// came back, so the panel can show "not read" instead of implying the device
    /// does not have them. Secret keys are absent because the firmware never
    /// requests them, not because they are filtered here.
    PublishStickerFullConfig {
        dev_eui: String,
        /// Flattened `group.field` → JSON value (projected from ConfigValue).
        config: BTreeMap<String, Value>,
        page_count: u32,
        last_seq: u32,
        /// `complete` when every requested key came back, else `partial`.
        read_status: String,
        /// Keys whose chunk failed — the gap, named.
        missing: Vec<String>,
    },

    /// Publish a STICKER's fPort-85 device info (#65) to
    /// `lorawan/sensors/<dev_eui>/info`, **retained**.
    ///
    /// Emitted both for an explicit `get_sticker_info` query and for the
    /// unsolicited `Response{seq=0, Info}` the sticker sends on every join, so
    /// `source` says which. `info` is already projected by
    /// `sticker_config::info_to_json` with `claim_token` redacted — never build
    /// this payload by hand.
    PublishStickerInfo { dev_eui: String, info: Value },

    /// Clear the retained device-info of a decommissioned sticker (#65). Sent when
    /// a sticker is removed: without it the broker keeps replaying a deleted
    /// device's info to every new subscriber.
    ClearStickerInfo { dev_eui: String },

    /// Publish the outcome of a STICKER control command (#71) to
    /// `lorawan/sensors/<dev_eui>/command`. Not retained — it is the result of one
    /// operator action, and replaying it to a new subscriber would look like a
    /// fresh command.
    PublishStickerCommandResult {
        dev_eui: String,
        command: String,
        seq: u32,
        /// `ok` | `requested` | a device error code | `transport_error` |
        /// `device_busy` | `rate_limited`.
        result: String,
        /// What still has to happen for the command to be observably complete, or
        /// `None` when the reply was final. Lets the viewer say "requested" instead
        /// of claiming success for a command that has no acknowledgement.
        expect: Option<String>,
        detail: Option<String>,
        fault_key: Option<String>,
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
    PublishEyeSensorData { tags: Vec<EyeTagPayload> },

    /// Publish the result of a detect_eye_tag probe (async; on `eye/detect`).
    /// `is_en12830` is `None` when the probe was inconclusive; `status` is
    /// "ok" | "unreachable" | "error".
    PublishEyeDetectResult {
        mac: String,
        is_en12830: Option<bool>,
        status: String,
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
    /// Hostname of the gateway that heard this tag (system#6).
    ///
    /// A tag can be in range of several FIBERs, so a reading is only meaningful
    /// together with who captured it. The topic carries the hostname too, but only
    /// when `mqtt.include_hostname` is enabled — an operator setting — so the
    /// payload states it unconditionally.
    pub gateway: String,
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
    /// `Some(true)` = EN12830 recorder (white) variant, `Some(false)` = standard
    /// (black), `None` = not yet determined. Surfaced so the viewer can label
    /// the tag type and expose recorder controls.
    pub is_en12830: Option<bool>,
    /// Per-field alarm state (field → NORMAL/WARNING/CRITICAL), evaluated on
    /// device from the tag's configured thresholds. Empty when none are set.
    pub field_alarm_states: std::collections::HashMap<String, String>,
    /// Aggregate (worst-of-fields) alarm state string.
    pub alarm_state: String,
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
    /// Every gateway that received the latest uplink, with its own RSSI/SNR.
    pub gateways: Vec<crate::libs::lorawan::chirpstack::GatewayRx>,
    /// LoRaWAN data-rate index of the latest uplink.
    pub dr: Option<i64>,
    /// Gateway ChirpStack used to transmit the last downlink (from `event/txack`).
    pub downlink_gateway_id: Option<String>,
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
        /// Vendor device-profile number (1-99) off the sticker's QR label, which
        /// is what fixes its region. Absent for manual entry and for viewers
        /// that pre-date it; see `resolve_otaa_profile`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_id: Option<u32>,
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
    GetSensorStatus {
        line: u8,
    },

    /// Switch display screen
    SetDisplayScreen {
        screen: String,
    },

    /// Flush storage to disk
    FlushStorage,

    /// Get device information
    GetDeviceInfo,

    /// Get sensor configuration (all 8 sensors)
    GetSensorConfig,

    /// Set sensor name (signed via ConfigRequest)
    SetSensorName {
        line: u8,
        name: String,
    },

    /// Set sensor probe location (signed via ConfigRequest)
    SetSensorLocation {
        line: u8,
        location: String,
    },

    /// Reboot the device at OS level. Unlike `PowerOffDevice` the unit comes
    /// back on its own, but the interruption is a gap in monitoring either way,
    /// so the executor flushes and audits before it goes down.
    ///
    /// `requested_by` is carried for the same reason as on `PowerOffDevice`: the
    /// executor never sees the signer, and the authorization audit lives on
    /// tmpfs (`/tmp/fiber_audit.db`, and the unit sets `PrivateTmp=true`), which
    /// a reboot wipes just as thoroughly as a power-off.
    RestartApplication {
        reason: String,
        requested_by: String,
    },

    /// Power the device off at OS level. Unlike `RestartApplication` the unit
    /// does not come back on its own — it has to be powered on by hand — so the
    /// executor flushes the audit trail before the rails drop.
    ///
    /// `requested_by` is carried in the command rather than read at the call
    /// site because the executor never sees the signer, and the authorization
    /// audit for this command lives on tmpfs (`/tmp/fiber_audit.db`) — the
    /// power-off destroys it. The durable row in the encrypted DB is the only
    /// surviving evidence of who took the monitoring function offline.
    PowerOffDevice {
        reason: String,
        requested_by: String,
    },

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

    /// Set screen idle timeout in seconds (signed via ConfigRequest).
    /// 0 disables the timeout (display always on).
    SetScreenTimeout {
        timeout_secs: u32,
    },

    /// Set buzzer volume (signed via ConfigRequest)
    /// 0 = muted, 1-100 = active (full volume)
    SetBuzzerVolume {
        volume: u8,
    },

    /// Replace the configured physical-display lines (signed via ConfigRequest).
    /// An empty list restores the built-in overview layout.
    SetDisplayLines {
        lines: Vec<crate::libs::config::DisplayLine>,
    },

    /// Silence buzzer (from alarm acknowledgment)
    /// Stops current pattern but re-arms for new alarms
    SilenceBuzzer,

    /// Set network configuration (signed via ConfigRequest)
    SetNetworkConfig {
        interface: String,   // "ethernet" or "wifi"
        config_type: String, // "dhcp" or "static"
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

    /// Set a single per-field threshold for a LoRaWAN sensor.
    ///
    /// `enabled: false` switches the alarm OFF for this field and is the only
    /// way to do so: an omitted bound inherits the YAML default, and the
    /// defaults arm temperature, humidity and the probe fields on every paired
    /// sticker. Absent from the request means `true`, so an older viewer keeps
    /// behaving exactly as before.
    SetLoRaWANFieldThreshold {
        dev_eui: String,
        field: String,
        critical_low: Option<f64>,
        warning_low: Option<f64>,
        warning_high: Option<f64>,
        critical_high: Option<f64>,
        enabled: bool,
    },

    /// Remove a per-field threshold for a LoRaWAN sensor
    DeleteLoRaWANFieldThreshold {
        dev_eui: String,
        field: String,
    },

    /// Set a single per-field alarm threshold for an EYE tag.
    SetEyeFieldThreshold {
        mac: String,
        field: String,
        critical_low: Option<f64>,
        warning_low: Option<f64>,
        warning_high: Option<f64>,
        critical_high: Option<f64>,
    },

    /// Remove a per-field alarm threshold for an EYE tag.
    DeleteEyeFieldThreshold {
        mac: String,
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

    /// Read every readable STICKER parameter, not just the settable ones
    /// (unsigned query). `GetStickerConfig` covers what the panel can write;
    /// this covers what it can only display — the `lorawan.*` identity group,
    /// the 1-Wire ROMs, `application.calibration` and `vendor_reset_allow`.
    ///
    /// Its own command and its own topic rather than `GetStickerConfig { keys }`
    /// with a wide list, because the two reads have different lifetimes: a
    /// Feature-C write republishes `config`, and the read-only snapshot must
    /// survive that.
    GetStickerFullConfig {
        dev_eui: String,
    },

    /// Read a STICKER's device info over MQTT (unsigned query, #65). One
    /// `GetInfo` downlink and one `Info` uplink — the cheapest sticker round trip
    /// there is, which is why it is a query rather than a signed command.
    GetStickerInfo {
        dev_eui: String,
    },

    /// Cold-reboot a STICKER (#71, signed). Acks, then restarts 8 s later,
    /// discarding any staged-but-unsaved config.
    StickerReboot {
        dev_eui: String,
    },

    /// Reset a STICKER to defaults, keeping identity and the LoRaWAN keys (#71,
    /// signed). Proto id 8 `device_reset` — the sticker stays joined, but every
    /// parameter and alarm rule is lost.
    ///
    /// Deliberately NOT called "factory reset": the real `factory_reset` (id 23) is
    /// NFC/shell-only and the device rejects it over the radio.
    StickerDeviceReset {
        dev_eui: String,
    },

    /// Clear a STICKER's pulse counters (#71, signed). Selective per channel —
    /// each flag is sent explicitly because the firmware treats an absent flag as
    /// "leave this counter alone".
    StickerResetCounters {
        dev_eui: String,
        hall_left: bool,
        hall_right: bool,
        input_a: bool,
        input_b: bool,
    },

    /// Ask a STICKER to report immediately (#71, unsigned). Changes no device
    /// state, so it is the same risk class as a read — it only costs airtime.
    /// Produces **no** fPort-85 reply: the telemetry uplink is the answer.
    StickerForceSend {
        dev_eui: String,
    },

    /// Set a STICKER's RTC (#71, signed). `unix_time = Some(..)` sets the clock
    /// directly and the device answers with an `Info` carrying the new time;
    /// `None` asks it to re-sync from the network instead, which produces no
    /// immediate reply and a deferred `Info` later.
    StickerClockSync {
        dev_eui: String,
        unix_time: Option<u32>,
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

    /// Switch the whole EYE BLE tag subsystem on or off (`eye.enabled`).
    ///
    /// Every other EYE command operates on a tag, so once the subsystem was off
    /// there was no way back except SSH — and the subsystem shipped off.
    SetEyeEnabled {
        enabled: bool,
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

    /// Register an EYE tag in the device config (`eye.tags[]`) so it is
    /// tracked/named explicitly. Auto-provisioning still discovers unknown
    /// tags; this pins a name/override. Signed via ConfigRequest.
    /// Replace the fleet allowlist of EYE MACs this gateway may listen for
    /// (system#6). Not a registration: ownership stays with whichever gateway has
    /// the tag in its own `eye.tags[]`.
    SetEyeKnownTags {
        macs: Vec<String>,
    },
    AddEyeTag {
        mac: String,
        name: Option<String>,
    },

    /// Remove an EYE tag from the device config (`eye.tags[]`). Signed via
    /// ConfigRequest.
    RemoveEyeTag {
        mac: String,
    },

    /// Connect to an EYE tag over BLE and determine whether it is an EN12830
    /// recorder (white) or a standard tag (black). Result surfaces
    /// asynchronously via `is_en12830` in the `eye/sensors` snapshot. Signed
    /// via ConfigRequest.
    DetectEyeTag {
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

    /// Add external LoRaWAN gateway: register in ChirpStack + save gateway config (signed via ConfigRequest)
    AddExternalGateway {
        gateway_eui: String,
        name: String,
    },

    /// Remove external LoRaWAN gateway: remove gateway config + deregister from ChirpStack (signed via ConfigRequest)
    RemoveExternalGateway {
        gateway_eui: String,
    },

    /// Set or clear this unit's role in a site-local LoRaWAN cluster
    /// (PROXIMOS system#7 Goal 2). `role` is "leader", "follower" or
    /// "standalone"; "standalone" clears the cluster and restores the shipped
    /// standalone behaviour.
    ///
    /// Every field is validated in
    /// `AuthorizationManager::build_command_from_challenge` — the only
    /// construction site — so the dispatch may rely on them. In particular
    /// `leader_host` is known to be site-local, `leader_port` is known not to be
    /// the anonymous loopback listener, `leader_ca_fingerprint` is known to match
    /// `leader_ca`, and `peer_username` is known to be a dedicated `peer-*`
    /// account rather than the leader's own shared credential.
    ///
    /// `MqttCommand` derives only `Debug`/`Clone` and is never serialised, so
    /// `peer_password` does not reach the broker or the logs.
    SetLorawanCluster {
        role: String,
        leader_host: Option<String>,
        leader_port: u16,
        leader_ca: Option<String>,
        leader_ca_fingerprint: Option<String>,
        peer_username: Option<String>,
        peer_password: Option<String>,
        /// Leader only: the follower's radio EUI, registered in this unit's
        /// ChirpStack so the peer's frames are accepted rather than dropped as
        /// coming from an unknown gateway.
        peer_gateway_eui: Option<String>,
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
    AddSigner {
        signer_data: Value,
    },

    /// Remove signer (signed via ConfigRequest)
    RemoveSigner {
        signer_id: String,
    },

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
            MqttCommand::PowerOffDevice { .. } => "power_off",
            MqttCommand::SetInterval { .. } => "set_interval",
            MqttCommand::GetInterval => "get_interval",
            MqttCommand::SetSystemInfoInterval { .. } => "set_system_info_interval",
            MqttCommand::SetDeviceLabel { .. } => "set_device_label",
            MqttCommand::SetLedBrightness { .. } => "set_led_brightness",
            MqttCommand::SetScreenBrightness { .. } => "set_screen_brightness",
            MqttCommand::SetScreenTimeout { .. } => "set_screen_timeout",
            MqttCommand::SetBuzzerVolume { .. } => "set_buzzer_volume",
            MqttCommand::SetDisplayLines { .. } => "set_display_lines",
            MqttCommand::SilenceBuzzer => "silence_buzzer",
            MqttCommand::SetNetworkConfig { .. } => "set_network_config",
            MqttCommand::SetLoRaWANSensorConfig { .. } => "set_lorawan_sensor_config",
            MqttCommand::SetLoRaWANFieldThreshold { .. } => "set_lorawan_field_threshold",
            MqttCommand::DeleteLoRaWANFieldThreshold { .. } => "delete_lorawan_field_threshold",
            MqttCommand::SetEyeFieldThreshold { .. } => "set_eye_field_threshold",
            MqttCommand::DeleteEyeFieldThreshold { .. } => "delete_eye_field_threshold",
            MqttCommand::AddLoRaWANSticker { .. } => "add_lorawan_sticker",
            MqttCommand::SetEyeEnabled { .. } => "set_eye_enabled",
            MqttCommand::SetEyeRecording { .. } => "set_eye_recording",
            MqttCommand::DownloadEyeHistory { .. } => "download_eye_history",
            MqttCommand::SetEyeKnownTags { .. } => "set_eye_known_tags",
            MqttCommand::AddEyeTag { .. } => "add_eye_tag",
            MqttCommand::RemoveEyeTag { .. } => "remove_eye_tag",
            MqttCommand::DetectEyeTag { .. } => "detect_eye_tag",
            MqttCommand::RemoveLoRaWANSticker { .. } => "remove_lorawan_sticker",
            MqttCommand::GetStickerConfig { .. } => "get_sticker_config",
            MqttCommand::GetStickerFullConfig { .. } => "get_sticker_full_config",
            MqttCommand::GetStickerInfo { .. } => "get_sticker_info",
            MqttCommand::StickerReboot { .. } => "sticker_reboot",
            MqttCommand::StickerDeviceReset { .. } => "sticker_device_reset",
            MqttCommand::StickerResetCounters { .. } => "sticker_reset_counters",
            MqttCommand::StickerForceSend { .. } => "sticker_force_send",
            MqttCommand::StickerClockSync { .. } => "sticker_clock_sync",
            MqttCommand::SetStickerConfig { .. } => "set_sticker_config",
            MqttCommand::SendStickerRaw { .. } => "send_sticker_raw",
            MqttCommand::GetStickerHistory { .. } => "get_sticker_history",
            MqttCommand::ResetExportCursor { .. } => "reset_export_cursor",
            MqttCommand::AddExternalGateway { .. } => "add_external_gateway",
            MqttCommand::RemoveExternalGateway { .. } => "remove_external_gateway",
            MqttCommand::SetLorawanCluster { .. } => "set_lorawan_cluster",
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
    /// Validate a sticker `dev_eui` out of a signed command's `params`.
    fn params_dev_eui(params: &Value) -> Result<String, String> {
        let dev_eui = params
            .get("dev_eui")
            .and_then(|v| v.as_str())
            .ok_or("Missing dev_eui")?;
        if dev_eui.len() != 16 || !dev_eui.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!(
                "Invalid dev_eui {:?} (expected 16 hex chars)",
                dev_eui
            ));
        }
        Ok(dev_eui.to_lowercase())
    }

    /// Parse `sticker_reboot` (#71).
    pub fn parse_sticker_reboot(params: &Value) -> Result<MqttCommand, String> {
        Ok(MqttCommand::StickerReboot {
            dev_eui: Self::params_dev_eui(params)?,
        })
    }

    /// Parse `sticker_device_reset` (#71).
    pub fn parse_sticker_device_reset(params: &Value) -> Result<MqttCommand, String> {
        Ok(MqttCommand::StickerDeviceReset {
            dev_eui: Self::params_dev_eui(params)?,
        })
    }

    /// Parse `sticker_force_send` (#71).
    pub fn parse_sticker_force_send(params: &Value) -> Result<MqttCommand, String> {
        Ok(MqttCommand::StickerForceSend {
            dev_eui: Self::params_dev_eui(params)?,
        })
    }

    /// Parse `sticker_reset_counters` (#71).
    ///
    /// Accepts either `{"counters": ["hall_left", ...]}` or `{"all": true}`.
    ///
    /// **An empty selection is rejected.** The firmware Acks a ResetCounters with
    /// no channels set while clearing nothing, so accepting one would report
    /// success for a guaranteed no-op. Requiring an explicit selection also makes
    /// the signed-command confirmation preview name the channels an operator is
    /// about to clear.
    pub fn parse_sticker_reset_counters(params: &Value) -> Result<MqttCommand, String> {
        let dev_eui = Self::params_dev_eui(params)?;
        const CHANNELS: [&str; 4] = ["hall_left", "hall_right", "input_a", "input_b"];

        if params.get("all").and_then(|v| v.as_bool()) == Some(true) {
            return Ok(MqttCommand::StickerResetCounters {
                dev_eui,
                hall_left: true,
                hall_right: true,
                input_a: true,
                input_b: true,
            });
        }

        let mut selected = [false; 4];
        if let Some(arr) = params.get("counters").and_then(|v| v.as_array()) {
            for entry in arr {
                let name = entry
                    .as_str()
                    .ok_or_else(|| "'counters' entries must be strings".to_string())?;
                match CHANNELS.iter().position(|c| *c == name) {
                    Some(i) => selected[i] = true,
                    None => {
                        return Err(format!(
                            "unknown counter {name:?} (expected one of {CHANNELS:?}); \
                             motion_count and accel_motion_count are RAM-only on the device \
                             and cannot be reset"
                        ))
                    }
                }
            }
        } else {
            // Accept per-channel booleans too, matching the proto field names.
            for (i, name) in CHANNELS.iter().enumerate() {
                if params.get(*name).and_then(|v| v.as_bool()) == Some(true) {
                    selected[i] = true;
                }
            }
        }

        if !selected.iter().any(|s| *s) {
            return Err(format!(
                "no counters selected; pass \"all\": true or a non-empty \"counters\" list \
                 from {CHANNELS:?} (an empty reset would be acknowledged but clear nothing)"
            ));
        }
        Ok(MqttCommand::StickerResetCounters {
            dev_eui,
            hall_left: selected[0],
            hall_right: selected[1],
            input_a: selected[2],
            input_b: selected[3],
        })
    }

    /// Parse `sticker_clock_sync` (#71).
    ///
    /// `unix_time` absent/null means "re-sync from the network" (no immediate
    /// reply, deferred `Info` later). A supplied value is range-checked against the
    /// firmware's own accepted window (2024-01-01 .. 2100-01-01,
    /// `APP_CMD_CLOCK_UNIX_MIN/MAX`) so a bad value fails here instead of burning a
    /// downlink to be told `BAD_REQUEST` "bad epoch".
    pub fn parse_sticker_clock_sync(params: &Value) -> Result<MqttCommand, String> {
        const CLOCK_MIN: u64 = 1_704_067_200; // 2024-01-01T00:00:00Z
        const CLOCK_MAX: u64 = 4_102_444_800; // 2100-01-01T00:00:00Z
        let dev_eui = Self::params_dev_eui(params)?;
        let unix_time = match params.get("unix_time") {
            None => None,
            Some(v) if v.is_null() => None,
            Some(v) => {
                let n = v
                    .as_u64()
                    .ok_or_else(|| "'unix_time' must be a number".to_string())?;
                if !(CLOCK_MIN..=CLOCK_MAX).contains(&n) {
                    return Err(format!(
                        "unix_time {n} outside the firmware's accepted range \
                         {CLOCK_MIN}..={CLOCK_MAX} (2024-01-01 .. 2100-01-01)"
                    ));
                }
                Some(n as u32)
            }
        };
        Ok(MqttCommand::StickerClockSync { dev_eui, unix_time })
    }

    pub fn parse_set_sticker_config(params: &Value) -> Result<MqttCommand, String> {
        let dev_eui = params
            .get("dev_eui")
            .and_then(|v| v.as_str())
            .ok_or("Missing dev_eui")?;
        if dev_eui.len() != 16 || !dev_eui.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!(
                "Invalid dev_eui {:?} (expected 16 hex chars)",
                dev_eui
            ));
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
        let save = params
            .get("save")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
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
            return Err(format!(
                "Invalid dev_eui {:?} (expected 16 hex chars)",
                dev_eui
            ));
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
            return Err(format!(
                "raw downlink too large: {} bytes (max 51)",
                bytes.len()
            ));
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
            MqttCommand::GetStickerConfig {
                dev_eui: "0102030405060708".into(),
                keys: None
            }
            .name(),
            "get_sticker_config"
        );
        // The viewer sends this literal string and subscribes to the matching
        // "/full-config" topic; a rename here silently dead-ends its read.
        assert_eq!(
            MqttCommand::GetStickerFullConfig {
                dev_eui: "0102030405060708".into()
            }
            .name(),
            "get_sticker_full_config"
        );
        let mut fields = BTreeMap::new();
        fields.insert(
            "application.interval_report".to_string(),
            "1200".to_string(),
        );
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
    fn parse_sticker_reset_counters_rejects_an_empty_selection() {
        // The firmware Acks a ResetCounters with no channels set while clearing
        // NOTHING, so accepting an empty selection would report success for a
        // guaranteed no-op. This is the host-side guard for that firmware quirk.
        let err = MqttCommand::parse_sticker_reset_counters(&serde_json::json!({
            "dev_eui": "70b3d57ed80051b2"
        }))
        .unwrap_err();
        assert!(err.contains("no counters selected"), "got {err:?}");

        let err = MqttCommand::parse_sticker_reset_counters(&serde_json::json!({
            "dev_eui": "70b3d57ed80051b2", "counters": []
        }))
        .unwrap_err();
        assert!(err.contains("no counters selected"), "got {err:?}");
    }

    #[test]
    fn parse_sticker_reset_counters_selects_channels() {
        match MqttCommand::parse_sticker_reset_counters(&serde_json::json!({
            "dev_eui": "70B3D57ED80051B2", "counters": ["hall_left", "input_b"]
        }))
        .unwrap()
        {
            MqttCommand::StickerResetCounters {
                dev_eui,
                hall_left,
                hall_right,
                input_a,
                input_b,
            } => {
                assert_eq!(dev_eui, "70b3d57ed80051b2");
                assert_eq!(
                    (hall_left, hall_right, input_a, input_b),
                    (true, false, false, true)
                );
            }
            other => panic!("wrong command: {}", other.name()),
        }

        match MqttCommand::parse_sticker_reset_counters(&serde_json::json!({
            "dev_eui": "70b3d57ed80051b2", "all": true
        }))
        .unwrap()
        {
            MqttCommand::StickerResetCounters {
                hall_left,
                hall_right,
                input_a,
                input_b,
                ..
            } => {
                assert_eq!(
                    (hall_left, hall_right, input_a, input_b),
                    (true, true, true, true)
                );
            }
            other => panic!("wrong command: {}", other.name()),
        }
    }

    #[test]
    fn parse_sticker_reset_counters_names_the_unresettable_counters() {
        // motion_count / accel_motion_count are RAM-only on the device, so asking
        // for them must fail with an explanation rather than silently doing nothing.
        let err = MqttCommand::parse_sticker_reset_counters(&serde_json::json!({
            "dev_eui": "70b3d57ed80051b2", "counters": ["motion_count"]
        }))
        .unwrap_err();
        assert!(err.contains("unknown counter"), "got {err:?}");
        assert!(
            err.contains("RAM-only"),
            "the error should explain why: {err:?}"
        );
    }

    #[test]
    fn parse_sticker_clock_sync_modes_and_range() {
        // Absent unix_time = "re-sync from the network": no immediate reply.
        match MqttCommand::parse_sticker_clock_sync(&serde_json::json!({
            "dev_eui": "70b3d57ed80051b2"
        }))
        .unwrap()
        {
            MqttCommand::StickerClockSync { unix_time, .. } => assert_eq!(unix_time, None),
            other => panic!("wrong command: {}", other.name()),
        }

        // A supplied time is accepted inside the firmware's own window.
        match MqttCommand::parse_sticker_clock_sync(&serde_json::json!({
            "dev_eui": "70b3d57ed80051b2", "unix_time": 1_782_198_249u64
        }))
        .unwrap()
        {
            MqttCommand::StickerClockSync { unix_time, .. } => {
                assert_eq!(unix_time, Some(1_782_198_249))
            }
            other => panic!("wrong command: {}", other.name()),
        }

        // Out of range fails here rather than burning a downlink to be told
        // BAD_REQUEST "bad epoch". 1600000000 is 2020, below APP_CMD_CLOCK_UNIX_MIN.
        let err = MqttCommand::parse_sticker_clock_sync(&serde_json::json!({
            "dev_eui": "70b3d57ed80051b2", "unix_time": 1_600_000_000u64
        }))
        .unwrap_err();
        assert!(
            err.contains("outside the firmware's accepted range"),
            "got {err:?}"
        );
    }

    #[test]
    fn control_command_names_are_stable() {
        // These strings are the MQTT wire contract with the viewer.
        let eui = "70b3d57ed80051b2".to_string();
        assert_eq!(
            MqttCommand::StickerReboot {
                dev_eui: eui.clone()
            }
            .name(),
            "sticker_reboot"
        );
        assert_eq!(
            MqttCommand::StickerDeviceReset {
                dev_eui: eui.clone()
            }
            .name(),
            "sticker_device_reset"
        );
        assert_eq!(
            MqttCommand::StickerForceSend {
                dev_eui: eui.clone()
            }
            .name(),
            "sticker_force_send"
        );
        assert_eq!(
            MqttCommand::StickerClockSync {
                dev_eui: eui.clone(),
                unix_time: None
            }
            .name(),
            "sticker_clock_sync"
        );
        assert_eq!(
            MqttCommand::StickerResetCounters {
                dev_eui: eui,
                hall_left: true,
                hall_right: true,
                input_a: true,
                input_b: true,
            }
            .name(),
            "sticker_reset_counters"
        );
    }

    #[test]
    fn parse_send_sticker_raw_validates_and_defaults() {
        use serde_json::json;
        assert_eq!(
            MqttCommand::SendStickerRaw {
                dev_eui: "0102030405060708".into(),
                bytes: vec![8],
                fport: 85
            }
            .name(),
            "send_sticker_raw"
        );
        // The docs.hardwario.com generator example (SetParam interval_report=600, save).
        let cmd = MqttCommand::parse_send_sticker_raw(
            &json!({ "dev_eui": "d7653371A0EF363F", "hex": "08011207120318d8041801" }),
        )
        .unwrap();
        match cmd {
            MqttCommand::SendStickerRaw {
                dev_eui,
                bytes,
                fport,
            } => {
                assert_eq!(dev_eui, "d7653371a0ef363f"); // lowercased
                assert_eq!(fport, 85); // default
                assert_eq!(
                    bytes,
                    vec![0x08, 0x01, 0x12, 0x07, 0x12, 0x03, 0x18, 0xd8, 0x04, 0x18, 0x01]
                );
            }
            _ => panic!("wrong variant"),
        }
        // fport override in range.
        assert!(matches!(
            MqttCommand::parse_send_sticker_raw(
                &json!({ "dev_eui": "0102030405060708", "hex": "08", "fport": 10 })
            )
            .unwrap(),
            MqttCommand::SendStickerRaw { fport: 10, .. }
        ));
        // Rejections: bad hex, bad dev_eui, empty, oversize (52 bytes), fport OOR.
        assert!(MqttCommand::parse_send_sticker_raw(
            &json!({ "dev_eui": "0102030405060708", "hex": "zz" })
        )
        .is_err());
        assert!(
            MqttCommand::parse_send_sticker_raw(&json!({ "dev_eui": "short", "hex": "08" }))
                .is_err()
        );
        assert!(MqttCommand::parse_send_sticker_raw(
            &json!({ "dev_eui": "0102030405060708", "hex": "" })
        )
        .is_err());
        assert!(MqttCommand::parse_send_sticker_raw(
            &json!({ "dev_eui": "0102030405060708", "hex": "aa".repeat(52) })
        )
        .is_err());
        assert!(MqttCommand::parse_send_sticker_raw(
            &json!({ "dev_eui": "0102030405060708", "hex": "08", "fport": 300 })
        )
        .is_err());
    }

    #[test]
    fn activation_mode_otaa_roundtrip() {
        let app_key: String = "ab".repeat(16);
        let join_eui: String = "cd".repeat(8);
        let v = ActivationMode::Otaa {
            app_key: app_key.clone(),
            join_eui: join_eui.clone(),
            profile_id: None,
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
        assert_eq!(
            s,
            serde_json::json!({
                "mode": "abp",
                "devaddr": "01020304",
                "nwkskey": nwkskey,
                "appskey": appskey,
            })
        );
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
