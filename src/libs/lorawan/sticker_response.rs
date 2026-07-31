//! In-app decoding of STICKER fPort-85 `Response` messages (command/response
//! protocol, #34). A `Response` carries the original command `seq` plus one of
//! Ack / Info (#65) / ConfigDump (#70) / HistoryFrame (#39) / Error / W1Scan.
//!
//! Like fPort 2/3, the wire payload is prefixed with the 1-byte
//! `APP_PROTO_VERSION`; the caller strips it before calling `decode_response`.

use std::collections::BTreeMap;

use prost::Message;

use super::sticker_proto::{response, Response};

/// One expanded history record (timestamp + decoded sensor values), produced
/// from a `HistoryFrame.samples` blob (#39).
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryRecord {
    /// Unix time of this record (`t0_unix + index * interval_s`), or `None` when
    /// the frame was captured before the device RTC synced (`t0_unix` is then
    /// uptime-relative, so an absolute timestamp would be a bogus ~1970 date).
    pub time: Option<u32>,
    /// Scaled analog values (temperature/humidity). Absent when the firmware
    /// wrote a sentinel (no valid sample for that channel in this record).
    pub fields: BTreeMap<String, f64>,
    /// Pulse counters (hall/input/motion).
    pub counters: BTreeMap<String, u64>,
}

#[derive(Clone, Copy)]
enum Enc {
    Temp,  // int16 LE, /100, sentinel 0x7fff
    Hum,   // uint8, /2, sentinel 0xff
    Count, // uint32 LE
}

/// `present`-mask bit order → (name, encoding). Mirrors `app_history_sensor`
/// enum in sticker-firmware `app_history.c` / the `ttn.js` `_HIST_SENSORS` table.
const HIST_SENSORS: &[(&str, Enc)] = &[
    ("temperature", Enc::Temp),
    ("humidity", Enc::Hum),
    ("s1_temperature", Enc::Temp),
    ("s1_humidity", Enc::Hum),
    ("s2_temperature", Enc::Temp),
    ("s2_humidity", Enc::Hum),
    ("s3_temperature", Enc::Temp),
    ("s3_humidity", Enc::Hum),
    ("s4_temperature", Enc::Temp),
    ("s4_humidity", Enc::Hum),
    ("hall_left_count", Enc::Count),
    ("hall_right_count", Enc::Count),
    ("input_a_count", Enc::Count),
    ("input_b_count", Enc::Count),
    ("motion_count", Enc::Count),
];

const HIST_TEMP_SENTINEL: u16 = 0x7fff;
const HIST_HUM_SENTINEL: u8 = 0xff;

/// Expand a `HistoryFrame.samples` blob into timestamped records. Each record is
/// a fixed-size, values-only run of the `present` channels (in bit order); record
/// `j`'s time is `t0_unix + j*interval_s`. Sentinel values decode to "absent".
/// `time_synced` (from `HistoryFrame.time_synced`, v1.4.0) is false when the frame
/// was captured before the RTC synced — then `t0_unix` is uptime-relative and each
/// record's `time` is `None` (mirrors `ttn.js`); absent field = old FW = synced.
pub fn expand_history_frame(
    t0_unix: u32,
    interval_s: u32,
    present: u32,
    samples: &[u8],
    time_synced: bool,
) -> Vec<HistoryRecord> {
    let mut rec_size = 0usize;
    for (s, (_, enc)) in HIST_SENSORS.iter().enumerate() {
        if present & (1 << s) == 0 {
            continue;
        }
        rec_size += match enc {
            Enc::Temp => 2,
            Enc::Hum => 1,
            Enc::Count => 4,
        };
    }
    if rec_size == 0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut p = 0usize;
    let mut j = 0u32;
    while p + rec_size <= samples.len() {
        let mut rec = HistoryRecord {
            time: if time_synced {
                Some(t0_unix.wrapping_add(j.wrapping_mul(interval_s)))
            } else {
                None
            },
            fields: BTreeMap::new(),
            counters: BTreeMap::new(),
        };
        for (k, (name, enc)) in HIST_SENSORS.iter().enumerate() {
            if present & (1 << k) == 0 {
                continue;
            }
            match enc {
                Enc::Temp => {
                    let raw = u16::from_le_bytes([samples[p], samples[p + 1]]);
                    p += 2;
                    if raw != HIST_TEMP_SENTINEL {
                        rec.fields.insert((*name).to_string(), raw as i16 as f64 / 100.0);
                    }
                }
                Enc::Hum => {
                    let hv = samples[p];
                    p += 1;
                    if hv != HIST_HUM_SENTINEL {
                        rec.fields.insert((*name).to_string(), hv as f64 / 2.0);
                    }
                }
                Enc::Count => {
                    let v = u32::from_le_bytes([
                        samples[p],
                        samples[p + 1],
                        samples[p + 2],
                        samples[p + 3],
                    ]);
                    p += 4;
                    rec.counters.insert((*name).to_string(), v as u64);
                }
            }
        }
        out.push(rec);
        j += 1;
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedResponse {
    /// Echoed command sequence number (correlates the reply to its request).
    pub seq: u32,
    pub kind: ResponseKind,
}

/// One latched alarm from `Response.Info.active_alarms` (proto field 15). The
/// numeric ids share the `app_alarm_source` / `app_alarm_quantity` enums used by
/// fPort-3 alarm reports, so `sticker_alarm`'s name tables resolve them.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveAlarm {
    pub source: u32,
    pub quantity: u32,
    /// `Response.AlarmStatus.type` — none/low/high/trigger/no_data.
    pub kind: u32,
}

/// A decoded `Response.Info` (fPort-85 `GetInfo` reply, and the unsolicited
/// `seq=0` Info the sticker emits on every join).
///
/// Its own struct rather than an inline enum variant: v1.4.0 takes this to 13
/// fields, and the previous inline form meant every exhaustive `match` pattern
/// across the crate broke whenever the firmware added one.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub fw_version: String,
    pub build_type: &'static str,
    pub serial_number: u32,
    pub uptime_s: u32,
    /// Wall clock (UTC seconds). `0` means the RTC is not synced yet — cross-check
    /// `device_status` bit 11 (`time_unsynced`).
    pub unix_time: u32,
    pub debug: bool,
    pub claim_token: Option<String>,
    /// Supply voltage in **mV** (`0` = unavailable). Deliberately not named
    /// `voltage`: the fPort-2 telemetry field of that name is in volts.
    pub battery_mv: u32,
    /// hwinfo reset-cause bitmask of the last boot (`0` = unknown).
    pub reset_cause: u32,
    /// `APP_DEVICE_STATUS_*` bitmask (`0` = all OK). Decode with
    /// [`device_status_flags`].
    pub device_status: u32,
    /// NFC-only in firmware v1.4.0 (`app_cmd.c:252-263`), so always `None` over
    /// LoRaWAN. Never render a default here — "idle" would be a fabrication.
    pub lrw_state: Option<&'static str>,
    /// NFC-only, as above. The LNS already knows the DevEUI from device context.
    pub dev_eui: Option<String>,
    /// Emitted on both transports; empty when nothing is latched.
    pub active_alarms: Vec<ActiveAlarm>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResponseKind {
    Ack,
    Info(DeviceInfo),
    ConfigDump {
        page_index: u32,
        page_count: u32,
        /// Decoded config read-back, flattened to `group.field` keys (#70).
        /// Only fields actually present in this page appear; secret LoRaWAN keys
        /// (nwkkey/appkey/nwkskey/appskey) are intentionally never surfaced.
        config: BTreeMap<String, ConfigValue>,
    },
    HistoryFrame {
        frame_index: u32,
        frame_count: u32,
        t0_unix: u32,
        present: u32,
        interval_s: u32,
        records: Vec<HistoryRecord>,
    },
    Error {
        code: &'static str,
        fault_field: u32,
        detail: String,
    },
    W1Scan {
        roms: Vec<String>,
    },
    /// `Response.sample` — a full `Telemetry` snapshot answering the `Sample`
    /// command (proto id 21).
    ///
    /// Unreachable over LoRaWAN by firmware design: `app_cmd_handle_sample`
    /// (`app_cmd.c:726-731`) only sets this body for `APP_CMD_TRANSPORT_NFC`,
    /// because a full `Telemetry` would not fit the 64-byte fPort-85 response
    /// buffer. Over the radio the fPort-2 frame is the answer instead. The variant
    /// exists so the decode match stays exhaustive and an unexpected sample is
    /// reported rather than mistaken for `Empty`.
    Sample,
    /// Response with no body set (forward-compat / unknown variant).
    Empty,
}

/// A single decoded config value. Mirrors `ttn.js` `_decodeCfgGroup` value
/// kinds: symbolic enum, scalar uint/bool, or raw bytes rendered as hex.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigValue {
    Bool(bool),
    Uint(u64),
    Enum(String),
    Hex(String),
}

/// One desired-vs-actual config disagreement (#70). `actual == None` means the
/// key was absent from the dumped config entirely.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigMismatch {
    pub key: String,
    pub desired: ConfigValue,
    pub actual: Option<ConfigValue>,
}

fn region_name(v: i32) -> &'static str {
    match v {
        0 => "EU868",
        1 => "US915",
        2 => "AU915",
        _ => "unknown",
    }
}

fn network_name(v: i32) -> &'static str {
    match v {
        0 => "PUBLIC",
        1 => "PRIVATE",
        _ => "unknown",
    }
}

fn activation_name(v: i32) -> &'static str {
    match v {
        0 => "OTAA",
        1 => "ABP",
        _ => "unknown",
    }
}

fn motion_name(v: i32) -> &'static str {
    match v {
        0 => "OFF",
        1 => "LOW",
        2 => "MEDIUM",
        3 => "HIGH",
        _ => "unknown",
    }
}

/// `AppConfigMessage.Lorawan.Mode` → symbolic name (v1.4.0, proto field 15).
fn mode_name(v: i32) -> &'static str {
    match v {
        0 => "OFF",
        1 => "LORAWAN",
        2 => "P2P",
        _ => "unknown",
    }
}

/// Flatten a `ConfigDump` page into `group.field` → value entries. Only fields
/// present on the wire are emitted (every config field is `optional`, so prost
/// preserves present-vs-absent). Mirrors the `ttn.js` config-decode name maps.
pub(crate) fn decode_config(c: &response::ConfigDump) -> BTreeMap<String, ConfigValue> {
    let mut m: BTreeMap<String, ConfigValue> = BTreeMap::new();

    macro_rules! ins_bool {
        ($src:expr, $grp:literal, $field:ident) => {
            if let Some(v) = $src.$field {
                m.insert(concat!($grp, ".", stringify!($field)).to_string(), ConfigValue::Bool(v));
            }
        };
    }
    macro_rules! ins_uint {
        ($src:expr, $grp:literal, $field:ident) => {
            if let Some(v) = $src.$field {
                m.insert(concat!($grp, ".", stringify!($field)).to_string(), ConfigValue::Uint(v as u64));
            }
        };
    }
    macro_rules! ins_enum {
        ($src:expr, $grp:literal, $field:ident, $namer:expr) => {
            if let Some(v) = $src.$field {
                m.insert(
                    concat!($grp, ".", stringify!($field)).to_string(),
                    ConfigValue::Enum($namer(v).to_string()),
                );
            }
        };
    }
    macro_rules! ins_hex {
        ($src:expr, $grp:literal, $field:ident) => {
            if let Some(v) = &$src.$field {
                m.insert(concat!($grp, ".", stringify!($field)).to_string(), ConfigValue::Hex(hex(v)));
            }
        };
    }

    if let Some(l) = &c.lorawan {
        ins_enum!(l, "lorawan", region, region_name);
        ins_uint!(l, "lorawan", sub_band);
        ins_enum!(l, "lorawan", network, network_name);
        ins_bool!(l, "lorawan", adr);
        ins_enum!(l, "lorawan", activation, activation_name);
        ins_hex!(l, "lorawan", deveui);
        ins_hex!(l, "lorawan", joineui);
        ins_hex!(l, "lorawan", devaddr);
        ins_uint!(l, "lorawan", link_check_interval);
        ins_uint!(l, "lorawan", link_check_fail_rejoin);
        ins_enum!(l, "lorawan", mode, mode_name);
        // nwkkey/appkey/nwkskey/appskey deliberately omitted (secrets).
    }
    if let Some(a) = &c.application {
        ins_bool!(a, "application", calibration);
        ins_uint!(a, "application", interval_sample);
        ins_uint!(a, "application", interval_report);
        ins_bool!(a, "application", history_enable);
        ins_uint!(a, "application", history_sensors);
        // battery_level is settable (#69), so it MUST decode — otherwise a
        // successful write reads back as absent and diff_config reports it
        // permanently unverified.
        ins_uint!(a, "application", battery_level);
        ins_bool!(a, "application", vendor_reset_allow);
    }
    if let Some(s) = &c.sensors {
        ins_bool!(s, "sensors", cap_hall_left);
        ins_bool!(s, "sensors", cap_hall_right);
        ins_bool!(s, "sensors", cap_input_a);
        ins_bool!(s, "sensors", cap_input_b);
        ins_bool!(s, "sensors", cap_light_sensor);
        ins_bool!(s, "sensors", cap_barometer);
        ins_bool!(s, "sensors", cap_pir_detector);
        ins_bool!(s, "sensors", cap_w1_sensors);
        ins_bool!(s, "sensors", cap_accelerometer);
        ins_enum!(s, "sensors", accel_motion_sensitivity, motion_name);
        ins_hex!(s, "sensors", sensor1_rom);
        ins_hex!(s, "sensors", sensor2_rom);
        ins_hex!(s, "sensors", sensor3_rom);
        ins_hex!(s, "sensors", sensor4_rom);
        ins_bool!(s, "sensors", hall_left_counter);
        ins_bool!(s, "sensors", hall_right_counter);
        ins_bool!(s, "sensors", input_a_counter);
        ins_bool!(s, "sensors", input_b_counter);
    }
    if let Some(al) = &c.alarms {
        ins_uint!(al, "alarms", alarm_limit);
        ins_uint!(al, "alarms", alarm_notif_time);
        ins_uint!(al, "alarms", alarm_light_confirm_delay);
        ins_hex!(al, "alarms", alarm_0);
        ins_hex!(al, "alarms", alarm_1);
        ins_hex!(al, "alarms", alarm_2);
        ins_hex!(al, "alarms", alarm_3);
        ins_hex!(al, "alarms", alarm_4);
        ins_hex!(al, "alarms", alarm_5);
        ins_hex!(al, "alarms", alarm_6);
        ins_hex!(al, "alarms", alarm_7);
        ins_hex!(al, "alarms", alarm_8);
        ins_hex!(al, "alarms", alarm_9);
        ins_hex!(al, "alarms", alarm_10);
        ins_hex!(al, "alarms", alarm_11);
        ins_hex!(al, "alarms", alarm_12);
        ins_hex!(al, "alarms", alarm_13);
        ins_hex!(al, "alarms", alarm_14);
        ins_hex!(al, "alarms", alarm_15);
    }
    m
}

/// Merge paged `ConfigDump`s (one `Response` per `page_index`) into a single
/// config map. Later pages override/extend earlier ones; a full dump is the
/// union of all `page_count` pages.
pub fn merge_config_dumps<'a, I>(pages: I) -> BTreeMap<String, ConfigValue>
where
    I: IntoIterator<Item = &'a BTreeMap<String, ConfigValue>>,
{
    let mut out = BTreeMap::new();
    for page in pages {
        for (k, v) in page {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// Compare a `desired` config against the device's `actual` (dumped) config.
/// Reports every desired key whose actual value differs or is missing. Keys
/// present only in `actual` are ignored (we only enforce what we intend to set).
pub fn diff_config(
    desired: &BTreeMap<String, ConfigValue>,
    actual: &BTreeMap<String, ConfigValue>,
) -> Vec<ConfigMismatch> {
    desired
        .iter()
        .filter_map(|(k, dv)| match actual.get(k) {
            Some(av) if av == dv => None,
            Some(av) => Some(ConfigMismatch {
                key: k.clone(),
                desired: dv.clone(),
                actual: Some(av.clone()),
            }),
            None => Some(ConfigMismatch { key: k.clone(), desired: dv.clone(), actual: None }),
        })
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn build_type_name(v: i32) -> &'static str {
    match v {
        0 => "main",
        1 => "dev",
        2 => "custom",
        _ => "unknown",
    }
}

/// `Response.Info.LrwState` → symbolic name (`app_lrw_state`).
fn lrw_state_name(v: i32) -> &'static str {
    match v {
        0 => "idle",
        1 => "joining",
        2 => "healthy",
        3 => "warning",
        4 => "reconnect",
        5 => "disabled",
        _ => "unknown",
    }
}

/// `Response.AlarmStatus.type` → symbolic name (`app_alarm_type`).
pub fn alarm_type_name(v: u32) -> &'static str {
    match v {
        0 => "none",
        1 => "low",
        2 => "high",
        3 => "trigger",
        4 => "no_data",
        _ => "unknown",
    }
}

/// Decode an `Info.device_status` bitmask into stable flag names.
///
/// Mirrors `APP_DEVICE_STATUS_*` (`app_cmd.h:39-50`). Bits 6 and 7 are reserved by
/// the firmware and have no name yet. **An unrecognised bit is reported as
/// `bitN`** rather than dropped, so a firmware that adds a status cannot go
/// silently unnoticed here.
pub fn device_status_flags(v: u32) -> Vec<String> {
    const NAMES: &[(u32, &str)] = &[
        (0, "alarm_any"),
        (1, "alarm_threshold"),
        (2, "alarm_state"),
        (3, "alarm_rate"),
        (4, "alarm_no_data"),
        (5, "alarm_low_batt"),
        (8, "nfc_down"),
        (9, "history_down"),
        (10, "i2c_wedged"),
        (11, "time_unsynced"),
        (12, "lrw_disabled"),
    ];
    let mut out = Vec::new();
    for bit in 0..32 {
        if v & (1 << bit) == 0 {
            continue;
        }
        match NAMES.iter().find(|(b, _)| *b == bit) {
            Some((_, name)) => out.push((*name).to_string()),
            None => out.push(format!("bit{bit}")),
        }
    }
    out
}

/// `Response.Error.Code` → the stable string the control socket and MQTT expose.
/// Mirrors the enum in `app_config.proto` @ sticker `v1.4.0`. A code this table
/// does not know collapses to "unknown", so a firmware that adds one needs a
/// line here before the UI can tell it apart.
fn error_code_name(v: i32) -> &'static str {
    match v {
        0 => "unknown",
        1 => "bad_request",
        2 => "out_of_range",
        3 => "not_ready",
        4 => "history_unavailable",
        5 => "unsupported_field",
        6 => "persist_failed",
        // NOT_SUPPORTED (L-54): the command tag is unknown to the firmware or was
        // removed. Distinct from bad_request — retrying cannot help.
        7 => "not_supported",
        // NOT_WRITABLE (M-3): the field exists but this transport may not write
        // it. Over LoRaWAN the whole `lorawan.*` group answers this way, and so
        // does `factory_reset` (id 23), which is NFC/shell-only.
        8 => "not_writable",
        _ => "unknown",
    }
}

/// Decode an fPort-85 `Response` (proto-version byte already stripped).
pub fn decode_response(bytes: &[u8]) -> Result<DecodedResponse, String> {
    let r = Response::decode(bytes).map_err(|e| format!("Response protobuf decode failed: {e}"))?;
    let kind = match r.body {
        Some(response::Body::Ack(_)) => ResponseKind::Ack,
        Some(response::Body::Info(i)) => ResponseKind::Info(DeviceInfo {
            fw_version: format!("{}.{}.{}", i.fw_major, i.fw_minor, i.fw_patch),
            build_type: build_type_name(i.build_type),
            serial_number: i.serial_number,
            uptime_s: i.uptime_s,
            unix_time: i.unix_time,
            debug: i.debug,
            claim_token: i.claim_token.as_deref().map(hex),
            battery_mv: i.battery,
            reset_cause: i.reset_cause,
            device_status: i.device_status,
            // Both are absent over LoRaWAN by firmware design, so map absent to
            // None instead of substituting a default that would read as fact.
            lrw_state: i.lrw_state.map(lrw_state_name),
            dev_eui: i.dev_eui.as_deref().map(hex),
            active_alarms: i
                .active_alarms
                .iter()
                .map(|a| ActiveAlarm { source: a.source, quantity: a.quantity, kind: a.r#type })
                .collect(),
        }),
        Some(response::Body::ConfigDump(c)) => ResponseKind::ConfigDump {
            page_index: c.page_index,
            page_count: c.page_count,
            config: decode_config(&c),
        },
        Some(response::Body::HistoryFrame(h)) => ResponseKind::HistoryFrame {
            frame_index: h.frame_index,
            frame_count: h.frame_count,
            t0_unix: h.t0_unix,
            present: h.present,
            interval_s: h.interval_s,
            // Absent time_synced (old FW) = synced, per firmware/ttn.js convention.
            records: expand_history_frame(
                h.t0_unix,
                h.interval_s,
                h.present,
                &h.samples,
                h.time_synced.unwrap_or(true),
            ),
        },
        Some(response::Body::Error(e)) => ResponseKind::Error {
            code: error_code_name(e.code),
            fault_field: e.fault_field,
            detail: e.detail,
        },
        Some(response::Body::W1Scan(w)) => ResponseKind::W1Scan {
            roms: w.rom.iter().map(|r| hex(r)).collect(),
        },
        // NFC-only over the wire (see ResponseKind::Sample); the payload is
        // deliberately dropped rather than decoded, since nothing here can ask
        // for it.
        Some(response::Body::Sample(_)) => ResponseKind::Sample,
        None => ResponseKind::Empty,
    };
    Ok(DecodedResponse { seq: r.seq, kind })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::lorawan::sticker_proto::app_config_message::{self, Application, Lorawan, Sensors};
    use crate::libs::lorawan::sticker_proto::{response, Response};

    #[test]
    fn config_dump_round_trip() {
        // A ConfigDump page carrying a lorawan + application group; verify the
        // flat key->value model (enum symbolic names, present-only fields).
        let resp = Response {
            seq: 5,
            body: Some(response::Body::ConfigDump(response::ConfigDump {
                page_index: 1,
                page_count: 2,
                lorawan: Some(Lorawan {
                    region: Some(app_config_message::lorawan::Region::Eu868 as i32),
                    activation: Some(app_config_message::lorawan::Activation::Otaa as i32),
                    adr: Some(true),
                    deveui: Some(vec![0x58, 0x76, 0x07, 0, 0, 0, 0, 0x01]),
                    ..Default::default()
                }),
                application: Some(Application {
                    interval_report: Some(3600),
                    history_enable: Some(true),
                    ..Default::default()
                }),
                sensors: None,
                alarms: None,
            })),
        };
        let d = decode_response(&resp.encode_to_vec()).unwrap();
        assert_eq!(d.seq, 5);
        match d.kind {
            ResponseKind::ConfigDump { page_index, page_count, config } => {
                assert_eq!((page_index, page_count), (1, 2));
                assert_eq!(config["lorawan.region"], ConfigValue::Enum("EU868".into()));
                assert_eq!(config["lorawan.activation"], ConfigValue::Enum("OTAA".into()));
                assert_eq!(config["lorawan.adr"], ConfigValue::Bool(true));
                assert_eq!(config["lorawan.deveui"], ConfigValue::Hex("5876070000000001".into()));
                assert_eq!(config["application.interval_report"], ConfigValue::Uint(3600));
                assert_eq!(config["application.history_enable"], ConfigValue::Bool(true));
                // absent fields must not appear
                assert!(!config.contains_key("lorawan.sub_band"));
                assert!(!config.contains_key("application.calibration"));
                // secrets are never surfaced even though the proto carries them
                assert!(!config.contains_key("lorawan.appkey"));
            }
            other => panic!("expected ConfigDump, got {other:?}"),
        }
    }

    #[test]
    fn real_e2e_fport85_config_dump_decodes() {
        // GOLDEN VECTOR: live fPort-85 Response{ConfigDump} from a STICKER,
        // captured as the on-wire payload (incl. 0x01 proto-version byte) the
        // firmware queues for fPort 85. Produced by a selective GetParam
        // requesting lorawan{region,adr,activation}, application{interval_report,
        // history_enable}, sensors{cap_hall_left} (GetParam, not GetConfig: the
        // full-dump GetConfig overflows the device stack in fw v1.4.0). Confirms
        // the in-app ConfigDump decode against real firmware output.
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        let raw = B64.decode("AQgJIhUQARoGCAAgASgAIgUYhAcgASoCCAE=").unwrap();
        let d = decode_response(&raw[1..]).unwrap(); // strip APP_PROTO_VERSION
        assert_eq!(d.seq, 9);
        match d.kind {
            ResponseKind::ConfigDump { page_index, page_count, config } => {
                assert_eq!((page_index, page_count), (0, 1));
                assert_eq!(config["lorawan.region"], ConfigValue::Enum("EU868".into()));
                assert_eq!(config["lorawan.adr"], ConfigValue::Bool(true));
                assert_eq!(config["lorawan.activation"], ConfigValue::Enum("OTAA".into()));
                assert_eq!(config["application.interval_report"], ConfigValue::Uint(900));
                assert_eq!(config["application.history_enable"], ConfigValue::Bool(true));
                assert_eq!(config["sensors.cap_hall_left"], ConfigValue::Bool(true));
                // nothing we didn't request leaked in
                assert_eq!(config.len(), 6);
            }
            other => panic!("expected ConfigDump, got {other:?}"),
        }
    }

    #[test]
    fn config_paging_merge() {
        // page 1: lorawan; page 2: sensors. The merged map is their union.
        let page1 = response::ConfigDump {
            page_index: 1,
            page_count: 2,
            lorawan: Some(Lorawan { adr: Some(true), ..Default::default() }),
            ..Default::default()
        };
        let page2 = response::ConfigDump {
            page_index: 2,
            page_count: 2,
            sensors: Some(Sensors { cap_hall_left: Some(true), ..Default::default() }),
            ..Default::default()
        };
        let m1 = decode_config(&page1);
        let m2 = decode_config(&page2);
        let merged = merge_config_dumps([&m1, &m2]);
        assert_eq!(merged["lorawan.adr"], ConfigValue::Bool(true));
        assert_eq!(merged["sensors.cap_hall_left"], ConfigValue::Bool(true));
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn config_diff_reports_mismatch_and_missing() {
        let desired = BTreeMap::from([
            ("application.interval_report".to_string(), ConfigValue::Uint(3600)),
            ("lorawan.adr".to_string(), ConfigValue::Bool(true)),
            ("sensors.cap_pir_detector".to_string(), ConfigValue::Bool(true)),
        ]);
        let actual = BTreeMap::from([
            ("application.interval_report".to_string(), ConfigValue::Uint(900)), // differs
            ("lorawan.adr".to_string(), ConfigValue::Bool(true)),               // matches
            // cap_pir_detector absent -> missing
            ("lorawan.region".to_string(), ConfigValue::Enum("EU868".into())),  // extra, ignored
        ]);
        let mut diffs = diff_config(&desired, &actual);
        diffs.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].key, "application.interval_report");
        assert_eq!(diffs[0].actual, Some(ConfigValue::Uint(900)));
        assert_eq!(diffs[1].key, "sensors.cap_pir_detector");
        assert_eq!(diffs[1].actual, None);
    }

    #[test]
    fn info_round_trip() {
        let resp = Response {
            seq: 7,
            body: Some(response::Body::Info(response::Info {
                fw_major: 1,
                fw_minor: 4,
                fw_patch: 0,
                build_type: 2,
                serial_number: 12345,
                uptime_s: 100,
                unix_time: 1_780_000_000,
                debug: true,
                claim_token: Some(vec![0xaa; 16]),
                // Fields 10-15, new in v1.4.0, left at their defaults so this test
                // keeps asserting exactly the pre-v1.4.0 subset. Their absence is
                // itself asserted below: a device that never sends them must not
                // acquire invented values.
                ..Default::default()
            })),
        };
        let d = decode_response(&resp.encode_to_vec()).unwrap();
        assert_eq!(d.seq, 7);
        match d.kind {
            ResponseKind::Info(i) => {
                assert_eq!(i.fw_version, "1.4.0");
                assert_eq!(i.build_type, "custom");
                assert_eq!(i.serial_number, 12345);
                assert!(i.debug);
                assert_eq!(i.claim_token.as_deref(), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
                // Backward compatibility: an Info without fields 10-15 decodes to
                // "unavailable", not to a fabricated reading.
                assert_eq!(i.battery_mv, 0);
                assert_eq!(i.reset_cause, 0);
                assert_eq!(i.device_status, 0);
                assert!(i.lrw_state.is_none(), "lrw_state is NFC-only; absent must stay absent");
                assert!(i.dev_eui.is_none(), "dev_eui is NFC-only; absent must stay absent");
                assert!(i.active_alarms.is_empty());
            }
            other => panic!("expected Info, got {other:?}"),
        }
    }

    #[test]
    fn info_v140_fields_and_status_flags_decode() {
        let resp = Response {
            seq: 21,
            body: Some(response::Body::Info(response::Info {
                fw_major: 1,
                fw_minor: 4,
                fw_patch: 0,
                build_type: 0,
                serial_number: 2_162_164_514,
                uptime_s: 1097,
                unix_time: 0, // RTC not synced — pairs with time_unsynced below
                debug: false,
                claim_token: None,
                battery: 2740,
                reset_cause: 1,
                // alarm_any (0) | alarm_low_batt (5) | time_unsynced (11)
                device_status: (1 << 0) | (1 << 5) | (1 << 11),
                lrw_state: None, // NFC-only, so absent over LoRaWAN
                dev_eui: None,
                active_alarms: vec![
                    response::AlarmStatus { source: 0, quantity: 0, r#type: 2 },
                    response::AlarmStatus { source: 11, quantity: 8, r#type: 1 },
                ],
            })),
        };
        let d = decode_response(&resp.encode_to_vec()).unwrap();
        match d.kind {
            ResponseKind::Info(i) => {
                assert_eq!(i.battery_mv, 2740);
                assert_eq!(i.reset_cause, 1);
                assert_eq!(i.unix_time, 0);
                assert!(i.claim_token.is_none());
                assert_eq!(
                    device_status_flags(i.device_status),
                    vec!["alarm_any", "alarm_low_batt", "time_unsynced"]
                );
                assert_eq!(i.active_alarms.len(), 2);
                assert_eq!(i.active_alarms[0], ActiveAlarm { source: 0, quantity: 0, kind: 2 });
                assert_eq!(alarm_type_name(i.active_alarms[0].kind), "high");
                // source 11 = battery, quantity 8 = voltage, type 1 = low
                assert_eq!(alarm_type_name(i.active_alarms[1].kind), "low");
            }
            other => panic!("expected Info, got {other:?}"),
        }
    }

    #[test]
    fn device_status_flags_names_known_bits_and_preserves_unknown() {
        assert!(device_status_flags(0).is_empty());
        assert_eq!(device_status_flags(1 << 11), vec!["time_unsynced"]);
        assert_eq!(device_status_flags(1 << 10), vec!["i2c_wedged"]);
        // Bits 6/7 are firmware-reserved and 13+ do not exist yet: an unnamed bit
        // must still reach the caller rather than being silently dropped.
        assert_eq!(device_status_flags(1 << 6), vec!["bit6"]);
        assert_eq!(device_status_flags(1 << 13), vec!["bit13"]);
        assert_eq!(device_status_flags(1 << 31), vec!["bit31"]);
        assert_eq!(
            device_status_flags((1 << 1) | (1 << 7) | (1 << 12)),
            vec!["alarm_threshold", "bit7", "lrw_disabled"]
        );
    }

    #[test]
    fn error_round_trip() {
        let resp = Response {
            seq: 3,
            body: Some(response::Body::Error(response::Error {
                code: 2,
                fault_field: 4,
                detail: "out of range".into(),
            })),
        };
        let d = decode_response(&resp.encode_to_vec()).unwrap();
        assert_eq!(d.seq, 3);
        assert_eq!(
            d.kind,
            ResponseKind::Error { code: "out_of_range", fault_field: 4, detail: "out of range".into() }
        );
    }

    #[test]
    fn v140_error_codes_not_supported_and_not_writable_decode() {
        // Both codes are new in sticker v1.4.0 and used on paths this branch adds:
        //   NOT_WRITABLE (8) — any `lorawan.*` write over LoRaWAN. fault_field is
        //     group-encoded, so 104 is lorawan (group 1) field 4 = adr.
        //   NOT_SUPPORTED (7) — an unknown/removed command tag.
        // Before this fix both decoded as "unknown", which made a permission
        // rejection indistinguishable from a firmware fault.
        let not_writable = Response {
            seq: 11,
            body: Some(response::Body::Error(response::Error {
                code: 8,
                fault_field: 104,
                detail: "transport not allowed".into(),
            })),
        };
        let d = decode_response(&not_writable.encode_to_vec()).unwrap();
        assert_eq!(
            d.kind,
            ResponseKind::Error {
                code: "not_writable",
                fault_field: 104,
                detail: "transport not allowed".into(),
            }
        );

        let not_supported = Response {
            seq: 12,
            body: Some(response::Body::Error(response::Error {
                code: 7,
                fault_field: 0,
                detail: "command not supported".into(),
            })),
        };
        let d = decode_response(&not_supported.encode_to_vec()).unwrap();
        assert!(matches!(d.kind, ResponseKind::Error { code: "not_supported", .. }));
    }

    #[test]
    fn real_e2e_fport85_info_decodes() {
        // GOLDEN VECTOR: live fPort-85 `Response{Info}` captured from a STICKER
        // (get_info reply), with the 0x01 proto-version byte stripped. Confirms
        // the in-app Response decode against real firmware output.
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        let raw = B64
            .decode("AQgDGikIARAEIAIoooaAhwgwigQ467vo0QZAAUoQFYpqXVtUxRGOYqj0rw3o0g==")
            .unwrap();
        let bytes = &raw[1..]; // strip APP_PROTO_VERSION
        let d = decode_response(bytes).unwrap();
        assert_eq!(d.seq, 3);
        match d.kind {
            ResponseKind::Info(i) => {
                // Asserted values unchanged from before the v1.4.0 widening — this
                // is the wire-contract canary.
                assert_eq!(i.fw_version, "1.4.0");
                assert_eq!(i.build_type, "custom");
                assert_eq!(i.serial_number, 2162164514);
                assert!(i.debug);
                assert_eq!(i.claim_token.as_deref(), Some("158a6a5d5b54c5118e62a8f4af0de8d2"));
                // This capture predates fields 10-15, so they must read as
                // unavailable rather than as a decode artefact.
                assert_eq!((i.battery_mv, i.reset_cause, i.device_status), (0, 0, 0));
                assert!(i.lrw_state.is_none() && i.dev_eui.is_none());
            }
            other => panic!("expected Info, got {other:?}"),
        }
    }

    #[test]
    fn history_expand_temp_hum() {
        // present = temperature(bit0) + humidity(bit1); record = 2B temp + 1B hum.
        let t0 = 1_780_000_000u32;
        let interval = 900u32;
        let present = 0b11u32;
        // rec0: temp 23.50 C (2350 = 0x092e LE), hum 50% (100 = 0x64)
        // rec1: temp -5.50 C (-550 = 0xfdda LE), hum sentinel 0xff -> absent
        let samples = [0x2e, 0x09, 0x64, 0xda, 0xfd, 0xff];
        let recs = expand_history_frame(t0, interval, present, &samples, true);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].time, Some(t0));
        assert_eq!(recs[0].fields["temperature"], 23.5);
        assert_eq!(recs[0].fields["humidity"], 50.0);
        assert_eq!(recs[1].time, Some(t0 + interval));
        assert_eq!(recs[1].fields["temperature"], -5.5);
        assert!(!recs[1].fields.contains_key("humidity")); // sentinel -> absent
    }

    #[test]
    fn history_expand_counter() {
        // present = motion_count (bit 14); record = 4B uint32 LE.
        let present = 1u32 << 14;
        let samples = [0x7b, 0x00, 0x00, 0x00]; // 123
        let recs = expand_history_frame(1000, 60, present, &samples, true);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].counters["motion_count"], 123);
        assert_eq!(recs[0].time, Some(1000));
    }

    #[test]
    fn history_expand_unsynced_time_is_null() {
        // time_synced=false (RTC unsynced at capture): t0_unix is uptime-relative,
        // so every record's time is None while the sample values still decode.
        let present = 0b11u32; // temperature + humidity
        let samples = [0x2e, 0x09, 0x64]; // 23.5 C, 50 %
        let recs = expand_history_frame(1000, 900, present, &samples, false);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].time, None);
        assert_eq!(recs[0].fields["temperature"], 23.5);
        assert_eq!(recs[0].fields["humidity"], 50.0);
    }

    #[test]
    fn history_frame_via_decode_response() {
        let samples = vec![0x2e, 0x09, 0x64]; // one record: 23.5 C, 50 %
        let resp = Response {
            seq: 11,
            body: Some(response::Body::HistoryFrame(response::HistoryFrame {
                frame_index: 0,
                frame_count: 1,
                t0_unix: 1_780_000_000,
                samples,
                present: 0b11,
                interval_s: 900,
                time_synced: None,
                next_ord: None,
                has_more: None,
            })),
        };
        let d = decode_response(&resp.encode_to_vec()).unwrap();
        assert_eq!(d.seq, 11);
        match d.kind {
            ResponseKind::HistoryFrame { records, frame_count, .. } => {
                assert_eq!(frame_count, 1);
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].fields["temperature"], 23.5);
                assert_eq!(records[0].fields["humidity"], 50.0);
            }
            other => panic!("expected HistoryFrame, got {other:?}"),
        }
    }

    #[test]
    fn real_e2e_fport85_unsolicited_info_on_join() {
        // GOLDEN VECTOR: the unsolicited device-info the STICKER sends as its FIRST
        // uplink after every join (fCnt=1, seq=0), captured live on a FIBER device
        // after `ats device reboot`. seq=0 => no pending command, so it is routed by
        // dev_eui rather than by seq correlation.
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        let raw = B64.decode("ARooCAEQBCACKKKGgIcIMBE4pPLo0QZAAUoQFYpqXVtUxRGOYqj0rw3o0g==").unwrap();
        let d = decode_response(&raw[1..]).unwrap(); // strip APP_PROTO_VERSION
        assert_eq!(d.seq, 0); // unsolicited
        match d.kind {
            ResponseKind::Info(i) => {
                assert_eq!(i.fw_version, "1.4.0");
                assert_eq!(i.build_type, "custom");
                assert_eq!(i.serial_number, 2162164514);
                assert!(i.debug);
                assert_eq!(i.claim_token.as_deref(), Some("158a6a5d5b54c5118e62a8f4af0de8d2"));
                // uptime_s is tiny here precisely because this is the post-reboot
                // join: 17 s, which is what makes the vector an unsolicited one.
                assert_eq!(i.uptime_s, 17);
            }
            other => panic!("expected Info, got {other:?}"),
        }
    }

    #[test]
    fn real_e2e_fport85_history_frame_decodes() {
        // GOLDEN VECTOR: live fPort-85 Response{HistoryFrame} captured from a
        // STICKER (ReqHistory reply) over RF -> local RAK gateway -> ChirpStack
        // on a FIBER device. Validates expand_history_frame against real output.
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        let raw = B64.decode("AQgBKhsQARjp3+jRBiILgAluAAAAAAAAAAAogxgwhAc=").unwrap();
        let d = decode_response(&raw[1..]).unwrap(); // strip APP_PROTO_VERSION
        assert_eq!(d.seq, 1);
        match d.kind {
            ResponseKind::HistoryFrame {
                frame_count, t0_unix, interval_s, present, records, ..
            } => {
                assert_eq!(frame_count, 1);
                assert_eq!(t0_unix, 1_782_198_249);
                assert_eq!(interval_s, 900);
                assert_eq!(present, 0xc03); // temperature+humidity+hall_left+hall_right
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].time, Some(1_782_198_249));
                assert_eq!(records[0].fields["temperature"], 24.32);
                assert_eq!(records[0].fields["humidity"], 55.0);
                assert_eq!(records[0].counters["hall_left_count"], 0);
                assert_eq!(records[0].counters["hall_right_count"], 0);
            }
            other => panic!("expected HistoryFrame, got {other:?}"),
        }
    }
}
