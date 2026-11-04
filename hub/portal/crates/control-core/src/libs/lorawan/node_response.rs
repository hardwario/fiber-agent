//! In-app decoding of NODE fPort-85 `Response` messages (command/response
//! protocol, #34). A `Response` carries the original command `seq` plus one of
//! Ack / Info (#65) / ConfigDump (#70) / HistoryFrame (#39) / Error / W1Scan.
//!
//! Like fPort 2/3, the wire payload is prefixed with the 1-byte
//! `APP_PROTO_VERSION`; the caller strips it before calling `decode_response`.

use std::collections::BTreeMap;

use control_radio::decode::history as radio_history;
use control_radio::decode::sensor_types;
use prost::Message;

use super::node_proto::{response, Response};

/// One expanded history record (timestamp + decoded sensor values), produced
/// from a `HistoryFrame.samples` blob (#39). The v2 radio's type: both stacks
/// expand a frame through `control_radio::decode::history`, so a replayed record
/// carries the same Hub field names whichever stack received it.
pub use control_radio::decode::history::HistoryRecord;

/// Expand a decoded `HistoryFrame` by its v2.0.0 `channels` / `w1_types` layout
/// (each column through the sensor type registry to its Hub field). A frame
/// without `channels` (pre-#430 firmware), or naming a column the registry does
/// not know, yields no records and is logged.
///
/// *Amended 2026-10-14 (v2.0.0 minimum):* the v1.4 `present` bit order and the
/// body's own `frame_index` / `frame_count` (before #425) are no longer read;
/// pages are numbered by the `Response` envelope.
pub fn expand_history(h: &response::HistoryFrame) -> Vec<HistoryRecord> {
    let frame = radio_history::HistoryFrame {
        t0_unix: h.t0_unix,
        samples: h.samples.clone(),
        interval_s: h.interval_s,
        // Absent time_synced (old FW) = synced, per firmware/ttn.js convention.
        time_synced: Some(h.time_synced.unwrap_or(true)),
        channels: h.channels.clone(),
        w1_types: (!h.w1_types.is_empty()).then(|| h.w1_types.clone()),
    };
    match frame.columns() {
        Some(Some(_)) => {}
        Some(None) => eprintln!(
            "[node] history frame with columns {:02x?} (w1_types {:02x?}) the sensor \
             type registry does not know: records not decoded",
            h.channels, h.w1_types
        ),
        None => eprintln!(
            "[node] history frame without channels (firmware older than v2.0.0): \
             records not decoded"
        ),
    }
    radio_history::expand(&frame)
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedResponse {
    /// Echoed command sequence number (correlates the reply to its request).
    pub seq: u32,
    pub kind: ResponseKind,
}

/// One latched alarm from `Response.Info.active_alarms` (proto field 15).
///
/// *Amended 2026-10-09 (sticker-firmware v2.0.0, #430):* addressed by
/// `(slot, channel)` of the sensor channel model, like the 18-byte rules and the
/// fPort-3 `AlarmEvent`; `node_alarm::{slot_name, channel_name, channel_field}`
/// resolve the ids through the sensor type registry.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveAlarm {
    /// 0 = motherboard, 1..4 = s1..s4 (was `source`).
    pub slot: u32,
    /// Channel of the slot's sensor type.
    pub channel: u32,
    /// `Response.AlarmStatus.type` — none/low/high/trigger/no_data/sensor_mismatch.
    pub kind: u32,
    /// Sensor type id of a 1-Wire slot; absent for slot 0.
    pub sensor_type: Option<u32>,
}

/// A decoded `Response.Info` (fPort-85 `GetInfo` reply, and the unsolicited
/// `seq=0` Info the node emits on every join).
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
    /// Gone from `Info` in v2.0.0 (field 12 `reserved`, now `get_radio_state`),
    /// so always `None`; kept so the published `lrw_state: null` shape holds.
    pub lrw_state: Option<&'static str>,
    /// NFC-only, as above. The LNS already knows the DevEUI from device context.
    pub dev_eui: Option<String>,
    /// Emitted on both transports; empty when nothing is latched.
    pub active_alarms: Vec<ActiveAlarm>,
    /// `Info.w1_slot_state` (v2.0.0, field 19), slots 1..4 in order, by name
    /// (`none`, `ok`, `absent`, `replaced`, `mismatch`, `state{N}` past the
    /// table). Empty when the Node omits it (every slot unused, or older FW).
    pub w1_slot_state: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResponseKind {
    Ack,
    Info(DeviceInfo),
    /// Numbered by the `Response` envelope (#425), `0` of `1` when unpaged.
    ConfigDump {
        page_index: u32,
        page_count: u32,
        /// Decoded config read-back, flattened to `group.field` keys (#70).
        /// Only fields actually present in this page appear; secret LoRaWAN keys
        /// (nwkkey/appkey/nwkskey/appskey) are intentionally never surfaced.
        config: BTreeMap<String, ConfigValue>,
        /// `ConfigDump.w1_slot_type` (field 7): the *detected* type of slots
        /// 1..4, runtime state rather than config, carried only by the boot
        /// settings-info dump. v2.0.0 ids by name: `empty`, `dallas`,
        /// `machine-probe`, else `type{N}`. Empty when the page has none.
        w1_slot_type: Vec<String>,
    },
    /// Numbered by the `Response` envelope (#425), `0` of `1` when unpaged.
    HistoryFrame {
        frame_index: u32,
        frame_count: u32,
        t0_unix: u32,
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

/// A proto enum value by its name in `names` (value order), `unknown` past it.
fn enum_name(names: &[&'static str], v: i32) -> &'static str {
    usize::try_from(v)
        .ok()
        .and_then(|i| names.get(i).copied())
        .unwrap_or("unknown")
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

/// `AppConfigMessage.Lorawan.RadioMode` → symbolic name (proto field 15; `Mode` before
/// sticker-firmware 7be011e).
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
///
/// Amended 2026-09-27 (proto refreshed from sticker-firmware 7be011e): lorawan
/// 13/14/15 are `radio_link_check_interval`, `radio_link_check_fail_rejoin` and
/// `radio_mode`, as the firmware's `ttn.js` names them (the old names are gone
/// with v2.0.0); the `p2p` group (field 8) is decoded;
/// `alarm_notif_time` (2) and `alarm_light_confirm_delay` (19) are `reserved` in
/// v2.0.0 and no longer exist on the wire. Alarms field 21 `radio_alarm_ack` is
/// sticker-firmware 3edb4a4's (#460 T2c).
///
/// Amended 2026-10-09 (sticker-firmware 435273c6, v2.0.0 #430):
/// `application.history_sensors` is gone; `history_channels` (24 B hex),
/// `interval_announce`, `sensors.cap_sht` and `sensors.sensor1..4_type` are
/// decoded, and `alarms.alarm_N` is an 18-byte rule (36 hex chars).
pub(crate) fn decode_config(c: &response::ConfigDump) -> BTreeMap<String, ConfigValue> {
    let mut m: BTreeMap<String, ConfigValue> = BTreeMap::new();

    macro_rules! ins_bool {
        ($src:expr, $grp:literal, $field:ident) => {
            if let Some(v) = $src.$field {
                m.insert(
                    concat!($grp, ".", stringify!($field)).to_string(),
                    ConfigValue::Bool(v),
                );
            }
        };
    }
    macro_rules! ins_uint {
        ($src:expr, $grp:literal, $field:ident) => {
            if let Some(v) = $src.$field {
                m.insert(
                    concat!($grp, ".", stringify!($field)).to_string(),
                    ConfigValue::Uint(v as u64),
                );
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
                m.insert(
                    concat!($grp, ".", stringify!($field)).to_string(),
                    ConfigValue::Hex(hex(v)),
                );
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
        ins_uint!(l, "lorawan", radio_link_check_interval);
        ins_uint!(l, "lorawan", radio_link_check_fail_rejoin);
        ins_enum!(l, "lorawan", radio_mode, mode_name);
        // nwkkey/appkey/nwkskey/appskey deliberately omitted (secrets).
    }
    if let Some(a) = &c.application {
        ins_bool!(a, "application", calibration);
        ins_uint!(a, "application", interval_sample);
        ins_uint!(a, "application", interval_report);
        ins_bool!(a, "application", history_enable);
        // `history_sensors` (5) is reserved since v2.0.0; `history_channels` (9)
        // lists the recorded columns, `interval_announce` (8) is #445.
        ins_hex!(a, "application", history_channels);
        ins_uint!(a, "application", interval_announce);
        // battery_level is settable (#69), so it MUST decode — otherwise a
        // successful write reads back as absent and diff_config reports it
        // permanently unverified.
        ins_uint!(a, "application", battery_level);
        ins_bool!(a, "application", vendor_reset_allow);
    }
    if let Some(s) = &c.sensors {
        ins_bool!(s, "sensors", cap_hall_left);
        ins_bool!(s, "sensors", cap_hall_right);
        ins_bool!(s, "sensors", cap_light_sensor);
        ins_bool!(s, "sensors", cap_barometer);
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
        ins_bool!(s, "sensors", cap_sht);
        ins_uint!(s, "sensors", sensor1_type);
        ins_uint!(s, "sensors", sensor2_type);
        ins_uint!(s, "sensors", sensor3_type);
        ins_uint!(s, "sensors", sensor4_type);
        ins_enum!(s, "sensors", cap_gpio_mode, |v| enum_name(
            super::node_command::GPIO_MODE_NAMES,
            v
        ));
        ins_enum!(s, "sensors", cap_gpio_input_a, |v| enum_name(
            super::node_command::GPIO_INPUT_NAMES,
            v
        ));
        ins_enum!(s, "sensors", cap_gpio_input_b, |v| enum_name(
            super::node_command::GPIO_INPUT_NAMES,
            v
        ));
    }
    if let Some(al) = &c.alarms {
        ins_uint!(al, "alarms", alarm_limit);
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
        ins_bool!(al, "alarms", radio_alarm_ack);
    }
    // P2P radio tuning (`ConfigDump` field 8, 7be011e). Read-only: shell-written.
    if let Some(p) = &c.p2p {
        ins_uint!(p, "p2p", frequency);
        ins_uint!(p, "p2p", spreading_factor);
        ins_uint!(p, "p2p", tx_power);
        ins_enum!(p, "p2p", modulation, |v| enum_name(&["LORA", "FSK"], v));
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
            None => Some(ConfigMismatch {
                key: k.clone(),
                desired: dv.clone(),
                actual: None,
            }),
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

/// `Response.AlarmStatus.type` / `AlarmEvent.type` → symbolic name.
pub fn alarm_type_name(v: u32) -> &'static str {
    match v {
        0 => "none",
        1 => "low",
        2 => "high",
        3 => "trigger",
        4 => "no_data",
        // v2.0.0 (#430): a 1-Wire slot holds a device of another type than its
        // `sensorN_type`.
        5 => "sensor_mismatch",
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
/// Mirrors the enum in `app_config.proto` @ node `v1.4.0`. A code this table
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
        // BUDGET_TOO_SMALL (#409, in the 7be011e proto): the answer does not fit
        // the current LoRaWAN data rate; retry once the DR rises.
        9 => "budget_too_small",
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
            // v2.0.0 no longer carries the link state in Info (field 12 is
            // `reserved`; it moved to `get_radio_state`, #446), so it stays None
            // rather than a default that would read as fact. `dev_eui` is absent
            // over LoRaWAN by firmware design.
            lrw_state: None,
            dev_eui: i.dev_eui.as_deref().map(hex),
            active_alarms: i
                .active_alarms
                .iter()
                .map(|a| ActiveAlarm {
                    slot: a.slot,
                    channel: a.channel,
                    kind: a.r#type,
                    sensor_type: a.sensor_type,
                })
                .collect(),
            w1_slot_state: i
                .w1_slot_state
                .iter()
                .map(|v| sensor_types::w1_slot_state_label(*v))
                .collect(),
        }),
        Some(response::Body::ConfigDump(c)) => ResponseKind::ConfigDump {
            page_index: r.page_index,
            page_count: r.page_count.max(1),
            config: decode_config(&c),
            w1_slot_type: c
                .w1_slot_type
                .iter()
                .map(|v| sensor_types::w1_type_label(*v))
                .collect(),
        },
        Some(response::Body::HistoryFrame(h)) => ResponseKind::HistoryFrame {
            frame_index: r.page_index,
            frame_count: r.page_count.max(1),
            t0_unix: h.t0_unix,
            interval_s: h.interval_s,
            records: expand_history(&h),
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
        // New with the 7be011e proto: claim_info / basic_info answer plain_text,
        // NFC and shell only, and radio_state answers a get_radio_state this
        // engine never sends. Before the refresh they were unknown fields and
        // decoded as no body; they still do.
        Some(response::Body::ClaimInfo(_))
        | Some(response::Body::BasicInfo(_))
        | Some(response::Body::RadioState(_))
        | None => ResponseKind::Empty,
    };
    Ok(DecodedResponse { seq: r.seq, kind })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::lorawan::node_proto::app_config_message::{
        self, Application, Lorawan, Sensors,
    };
    use crate::libs::lorawan::node_proto::{response, Response};

    #[test]
    fn config_dump_round_trip() {
        // A ConfigDump page carrying a lorawan + application group; verify the
        // flat key->value model (enum symbolic names, present-only fields).
        let resp = Response {
            seq: 5,
            page_index: 1,
            page_count: 2,
            body: Some(response::Body::ConfigDump(response::ConfigDump {
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
                ..Default::default()
            })),
        };
        let d = decode_response(&resp.encode_to_vec()).unwrap();
        assert_eq!(d.seq, 5);
        match d.kind {
            ResponseKind::ConfigDump {
                page_index,
                page_count,
                config,
                ..
            } => {
                assert_eq!((page_index, page_count), (1, 2));
                assert_eq!(config["lorawan.region"], ConfigValue::Enum("EU868".into()));
                assert_eq!(
                    config["lorawan.activation"],
                    ConfigValue::Enum("OTAA".into())
                );
                assert_eq!(config["lorawan.adr"], ConfigValue::Bool(true));
                assert_eq!(
                    config["lorawan.deveui"],
                    ConfigValue::Hex("5876070000000001".into())
                );
                assert_eq!(
                    config["application.interval_report"],
                    ConfigValue::Uint(3600)
                );
                assert_eq!(
                    config["application.history_enable"],
                    ConfigValue::Bool(true)
                );
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
        // GOLDEN VECTOR: live fPort-85 Response{ConfigDump} from a NODE,
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
            ResponseKind::ConfigDump {
                page_index,
                page_count,
                config,
                ..
            } => {
                assert_eq!((page_index, page_count), (0, 1));
                assert_eq!(config["lorawan.region"], ConfigValue::Enum("EU868".into()));
                assert_eq!(config["lorawan.adr"], ConfigValue::Bool(true));
                assert_eq!(
                    config["lorawan.activation"],
                    ConfigValue::Enum("OTAA".into())
                );
                assert_eq!(
                    config["application.interval_report"],
                    ConfigValue::Uint(900)
                );
                assert_eq!(
                    config["application.history_enable"],
                    ConfigValue::Bool(true)
                );
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
            lorawan: Some(Lorawan {
                adr: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let page2 = response::ConfigDump {
            sensors: Some(Sensors {
                cap_hall_left: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let m1 = decode_config(&page1);
        let m2 = decode_config(&page2);
        let merged = merge_config_dumps([&m1, &m2]);
        assert_eq!(merged["lorawan.adr"], ConfigValue::Bool(true));
        assert_eq!(merged["sensors.cap_hall_left"], ConfigValue::Bool(true));
        assert_eq!(merged.len(), 2);
    }

    /// Bench PS-1 (sticker-firmware 7be011e): the renamed lorawan 13/14/15 and the
    /// `p2p` group (`ConfigDump` field 8) — pages 1, 4 and 5 of STICKER 0413's dump.
    #[test]
    fn a_7be011e_dump_decodes_the_p2p_group_and_the_radio_names() {
        let hex = |s: &str| -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        };
        let mut config = BTreeMap::new();
        for page in [
            "01083722181a16080010021800200128003208587607000000041378026806",
            "010837221f1a173a0800000000000000005204000000006806700580010022040800103c60016806",
            "010837222232161a1103000000000000704100000c4200000000a00100420808a0cff89d03100760046806",
            "01083722044202180e60056806",
        ] {
            let d = decode_response(&hex(page)[1..]).unwrap();
            let ResponseKind::ConfigDump { config: c, .. } = d.kind else {
                panic!("a ConfigDump page");
            };
            config.extend(c);
        }
        assert_eq!(
            config["lorawan.radio_link_check_interval"],
            ConfigValue::Uint(6)
        );
        assert_eq!(
            config["lorawan.radio_link_check_fail_rejoin"],
            ConfigValue::Uint(5)
        );
        assert_eq!(
            config["lorawan.radio_mode"],
            ConfigValue::Enum("P2P".into())
        );
        assert_eq!(config["p2p.frequency"], ConfigValue::Uint(868_100_000));
        assert_eq!(config["p2p.spreading_factor"], ConfigValue::Uint(7));
        assert_eq!(config["p2p.tx_power"], ConfigValue::Uint(14));
        assert!(!config.contains_key("lorawan.link_check_interval"));
    }

    /// sticker-firmware 3edb4a4 (#460 T2c) dumps alarms field 21, `radio_alarm_ack`.
    #[test]
    fn a_dump_decodes_radio_alarm_ack() {
        let page = response::ConfigDump {
            alarms: Some(app_config_message::Alarms {
                radio_alarm_ack: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let config = decode_config(&page);
        assert_eq!(config["alarms.radio_alarm_ack"], ConfigValue::Bool(false));
        assert_eq!(config.len(), 1);
    }

    #[test]
    fn config_diff_reports_mismatch_and_missing() {
        let desired = BTreeMap::from([
            (
                "application.interval_report".to_string(),
                ConfigValue::Uint(3600),
            ),
            ("lorawan.adr".to_string(), ConfigValue::Bool(true)),
            (
                "sensors.cap_gpio_mode".to_string(),
                ConfigValue::Enum("PIR".into()),
            ),
        ]);
        let actual = BTreeMap::from([
            (
                "application.interval_report".to_string(),
                ConfigValue::Uint(900),
            ), // differs
            ("lorawan.adr".to_string(), ConfigValue::Bool(true)), // matches
            // cap_gpio_mode absent -> missing
            (
                "lorawan.region".to_string(),
                ConfigValue::Enum("EU868".into()),
            ), // extra, ignored
        ]);
        let mut diffs = diff_config(&desired, &actual);
        diffs.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].key, "application.interval_report");
        assert_eq!(diffs[0].actual, Some(ConfigValue::Uint(900)));
        assert_eq!(diffs[1].key, "sensors.cap_gpio_mode");
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
            ..Default::default()
        };
        let d = decode_response(&resp.encode_to_vec()).unwrap();
        assert_eq!(d.seq, 7);
        match d.kind {
            ResponseKind::Info(i) => {
                assert_eq!(i.fw_version, "1.4.0");
                assert_eq!(i.build_type, "custom");
                assert_eq!(i.serial_number, 12345);
                assert!(i.debug);
                assert_eq!(
                    i.claim_token.as_deref(),
                    Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                );
                // Backward compatibility: an Info without fields 10-15 decodes to
                // "unavailable", not to a fabricated reading.
                assert_eq!(i.battery_mv, 0);
                assert_eq!(i.reset_cause, 0);
                assert_eq!(i.device_status, 0);
                assert!(
                    i.lrw_state.is_none(),
                    "lrw_state is gone from Info (v2.0.0); absent must stay absent"
                );
                assert!(
                    i.dev_eui.is_none(),
                    "dev_eui is NFC-only; absent must stay absent"
                );
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
                dev_eui: None,
                active_alarms: vec![
                    response::AlarmStatus {
                        slot: 0,
                        r#type: 2,
                        channel: 0,
                        sensor_type: None,
                    },
                    response::AlarmStatus {
                        slot: 0,
                        r#type: 1,
                        channel: 20,
                        sensor_type: None,
                    },
                ],
                ..Default::default()
            })),
            ..Default::default()
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
                assert_eq!(
                    i.active_alarms[0],
                    ActiveAlarm {
                        slot: 0,
                        channel: 0,
                        kind: 2,
                        sensor_type: None,
                    }
                );
                assert_eq!(alarm_type_name(i.active_alarms[0].kind), "high");
                // slot 0 channel 20 = battery-voltage, type 1 = low
                assert_eq!(i.active_alarms[1].channel, 20);
                assert_eq!(alarm_type_name(i.active_alarms[1].kind), "low");
                assert!(i.w1_slot_state.is_empty(), "omitted, not invented");
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
            ..Default::default()
        };
        let d = decode_response(&resp.encode_to_vec()).unwrap();
        assert_eq!(d.seq, 3);
        assert_eq!(
            d.kind,
            ResponseKind::Error {
                code: "out_of_range",
                fault_field: 4,
                detail: "out of range".into()
            }
        );
    }

    #[test]
    fn v140_error_codes_not_supported_and_not_writable_decode() {
        // Both codes are new in node v1.4.0 and used on paths this branch adds:
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
            ..Default::default()
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
            ..Default::default()
        };
        let d = decode_response(&not_supported.encode_to_vec()).unwrap();
        assert!(matches!(
            d.kind,
            ResponseKind::Error {
                code: "not_supported",
                ..
            }
        ));
    }

    #[test]
    fn real_e2e_fport85_info_decodes() {
        // GOLDEN VECTOR: live fPort-85 `Response{Info}` captured from a NODE
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
                assert_eq!(
                    i.claim_token.as_deref(),
                    Some("158a6a5d5b54c5118e62a8f4af0de8d2")
                );
                // This capture predates fields 10-15, so they must read as
                // unavailable rather than as a decode artefact.
                assert_eq!((i.battery_mv, i.reset_cause, i.device_status), (0, 0, 0));
                assert!(i.lrw_state.is_none() && i.dev_eui.is_none());
            }
            other => panic!("expected Info, got {other:?}"),
        }
    }

    #[test]
    fn history_frame_via_decode_response() {
        let samples = vec![0x2e, 0x09, 0x64]; // one record: 23.5 C, 50 %
        let resp = Response {
            seq: 11,
            page_index: 1,
            page_count: 3,
            body: Some(response::Body::HistoryFrame(response::HistoryFrame {
                t0_unix: 1_780_000_000,
                samples,
                channels: vec![0x00, 0x01], // mb temperature, humidity
                interval_s: 900,
                time_synced: None,
                next_ord: None,
                has_more: None,
                ..Default::default()
            })),
        };
        let d = decode_response(&resp.encode_to_vec()).unwrap();
        assert_eq!(d.seq, 11);
        match d.kind {
            ResponseKind::HistoryFrame {
                records,
                frame_index,
                frame_count,
                ..
            } => {
                assert_eq!((frame_index, frame_count), (1, 3), "the envelope's");
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].fields["temperature"], 23.5);
                assert_eq!(records[0].fields["humidity"], 50.0);
            }
            other => panic!("expected HistoryFrame, got {other:?}"),
        }
    }

    #[test]
    fn real_e2e_fport85_unsolicited_info_on_join() {
        // GOLDEN VECTOR: the unsolicited device-info the NODE sends as its FIRST
        // uplink after every join (fCnt=1, seq=0), captured live on a FIBER device
        // after `ats device reboot`. seq=0 => no pending command, so it is routed by
        // dev_eui rather than by seq correlation.
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        let raw = B64
            .decode("ARooCAEQBCACKKKGgIcIMBE4pPLo0QZAAUoQFYpqXVtUxRGOYqj0rw3o0g==")
            .unwrap();
        let d = decode_response(&raw[1..]).unwrap(); // strip APP_PROTO_VERSION
        assert_eq!(d.seq, 0); // unsolicited
        match d.kind {
            ResponseKind::Info(i) => {
                assert_eq!(i.fw_version, "1.4.0");
                assert_eq!(i.build_type, "custom");
                assert_eq!(i.serial_number, 2162164514);
                assert!(i.debug);
                assert_eq!(
                    i.claim_token.as_deref(),
                    Some("158a6a5d5b54c5118e62a8f4af0de8d2")
                );
                // uptime_s is tiny here precisely because this is the post-reboot
                // join: 17 s, which is what makes the vector an unsolicited one.
                assert_eq!(i.uptime_s, 17);
            }
            other => panic!("expected Info, got {other:?}"),
        }
    }

    /// The pre-#430 frame a v1.4 NODE answered ReqHistory with (captured over RF):
    /// its `present` layout and body numbering are no longer read, so it decodes
    /// as one unpaged frame with no records.
    #[test]
    fn a_pre_v200_history_frame_yields_no_records() {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        let raw = B64
            .decode("AQgBKhsQARjp3+jRBiILgAluAAAAAAAAAAAogxgwhAc=")
            .unwrap();
        let d = decode_response(&raw[1..]).unwrap(); // strip APP_PROTO_VERSION
        match d.kind {
            ResponseKind::HistoryFrame {
                frame_index,
                frame_count,
                t0_unix,
                records,
                ..
            } => {
                assert_eq!((frame_index, frame_count), (0, 1));
                assert_eq!(t0_unix, 1_782_198_249);
                assert!(records.is_empty());
            }
            other => panic!("expected HistoryFrame, got {other:?}"),
        }
    }

    /// v2.0.0 (#430): `Info.w1_slot_state` (19) and an `AlarmStatus` on a 1-Wire
    /// slot carry the sensor channel model.
    #[test]
    fn info_v200_slot_state_and_w1_alarm_decode() {
        let resp = Response {
            seq: 0,
            body: Some(response::Body::Info(response::Info {
                fw_major: 1,
                fw_minor: 5,
                active_alarms: vec![response::AlarmStatus {
                    slot: 2,
                    r#type: 5,
                    channel: 0,
                    sensor_type: Some(3),
                }],
                w1_slot_state: vec![1, 4, 0, 9],
                ..Default::default()
            })),
            ..Default::default()
        };
        let ResponseKind::Info(i) = decode_response(&resp.encode_to_vec()).unwrap().kind else {
            panic!("an Info");
        };
        assert_eq!(i.w1_slot_state, ["ok", "mismatch", "none", "state9"]);
        assert_eq!(
            i.active_alarms,
            [ActiveAlarm {
                slot: 2,
                channel: 0,
                kind: 5,
                sensor_type: Some(3),
            }]
        );
        assert_eq!(alarm_type_name(5), "sensor_mismatch");
    }

    /// v2.0.0 (#430): the new config keys decode, the detected slot types use
    /// the registry ids (dallas moved 1 -> 2, machine-probe 2 -> 3).
    #[test]
    fn a_v200_dump_decodes_the_new_keys_and_slot_types() {
        let mut channels = vec![0x00, 0x01, 0x20];
        channels.resize(24, 0xff);
        let resp = Response {
            seq: 0,
            body: Some(response::Body::ConfigDump(response::ConfigDump {
                application: Some(Application {
                    interval_announce: Some(24),
                    history_channels: Some(channels),
                    ..Default::default()
                }),
                sensors: Some(Sensors {
                    cap_sht: Some(true),
                    sensor1_type: Some(3),
                    ..Default::default()
                }),
                alarms: Some(app_config_message::Alarms {
                    alarm_0: Some(vec![0; 18]),
                    ..Default::default()
                }),
                w1_slot_type: vec![0, 2, 3, 1, 7],
                ..Default::default()
            })),
            ..Default::default()
        };
        let ResponseKind::ConfigDump {
            config,
            w1_slot_type,
            ..
        } = decode_response(&resp.encode_to_vec()).unwrap().kind
        else {
            panic!("a ConfigDump");
        };
        assert_eq!(
            w1_slot_type,
            ["empty", "dallas", "machine-probe", "type1", "type7"]
        );
        assert_eq!(
            config["application.interval_announce"],
            ConfigValue::Uint(24)
        );
        assert_eq!(
            config["application.history_channels"],
            ConfigValue::Hex(format!("000120{}", "ff".repeat(21)))
        );
        assert_eq!(config["sensors.cap_sht"], ConfigValue::Bool(true));
        assert_eq!(config["sensors.sensor1_type"], ConfigValue::Uint(3));
        assert_eq!(config["alarms.alarm_0"], ConfigValue::Hex("00".repeat(18)));
        assert!(!config.contains_key("application.history_sensors"));
    }

    /// v2.0.0 (#430): a frame with `channels` lays its records out by the
    /// registry and names each column by its Hub field.
    #[test]
    fn a_v200_history_frame_decodes_by_channels() {
        // Columns: board temperature (i16/100), s1 machine-probe temperature-aux
        // (i16/100), board hall-left-count (u32 counter).
        let resp = Response {
            seq: 4,
            body: Some(response::Body::HistoryFrame(response::HistoryFrame {
                t0_unix: 1_790_000_000,
                interval_s: 600,
                present: 0b111,
                channels: vec![0x00, 0x22, 0x06],
                w1_types: vec![3, 0, 0, 0],
                // rec0: 21.50, 19.25, 7 · rec1: absent, -1.00, 8
                samples: vec![
                    0x66, 0x08, 0x85, 0x07, 7, 0, 0, 0, //
                    0xff, 0x7f, 0x9c, 0xff, 8, 0, 0, 0,
                ],
                ..Default::default()
            })),
            ..Default::default()
        };
        let ResponseKind::HistoryFrame { records, .. } =
            decode_response(&resp.encode_to_vec()).unwrap().kind
        else {
            panic!("a HistoryFrame");
        };
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].time, Some(1_790_000_000));
        assert_eq!(records[0].fields["temperature"], 21.5);
        assert_eq!(records[0].fields["w1_1_temperature_aux"], 19.25);
        assert_eq!(records[0].counters["hall_left_count"], 7);
        assert!(!records[1].fields.contains_key("temperature"));
        assert_eq!(records[1].fields["w1_1_temperature_aux"], -1.0);
        assert_eq!(records[1].time, Some(1_790_000_600));

        // A column the registry does not know: no records, not a guess.
        let unknown = response::HistoryFrame {
            channels: vec![0x00, 0x1f],
            samples: vec![0; 8],
            ..Default::default()
        };
        assert!(expand_history(&unknown).is_empty());
    }
}
