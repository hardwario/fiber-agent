//! Node-reported state — `plan/control/radio/node_reported_state.md` §2.
//!
//! What a Node says about *itself* over fPort 85, as the two retained documents the
//! northbound carries: `device_info` (the last `Info`) and `config` (the last complete
//! `ConfigDump`). Both are state rather than events, so each carries the `received_at` a
//! consumer needs to present it as cached, never as a live read (devices.node.md UI-9).
//!
//! The types are the published envelope, shared with the consumer the same way
//! [`crate::decode::reading::NodeReading`] is: `portal` deserializes these structs rather than
//! mirroring them, so the two ends cannot drift.
//!
//! # Rendering a `ConfigDump`
//!
//! [`render_config_dump`] turns the dump's group submessages into `group.field` keys through
//! the encoder's own table ([`command_encode::key_by_wire`]) — the vocabulary `set-config`
//! writes, the portal's config card edits and its offline detector reads. A second name table
//! here would be free to disagree with the one that writes the same keys. Rules (§2.2):
//! key material ([`ValueKind::Secret`]) is never rendered; an unknown `(group, field)` is not
//! guessed at and survives only in `body_hex`; an absent key is "not reported", never its
//! proto3 default, because a dump can carry a subset of the groups.
//!
//! *Amended 2026-09-27 (sticker-firmware 7be011e):* the `p2p` group (`ConfigDump` field 8)
//! renders as `p2p.frequency`, `p2p.spreading_factor`, `p2p.tx_power`; before, it was dropped
//! as an unknown field (bench PS-1). Lorawan 13/14 render as `lorawan.radio_link_check_*`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::decode::command::hex;
use crate::decode::command_encode::{self, ConfigGroup, ValueKind};
use crate::decode::pb::{read_tag, read_varint, skip};

/// `Response.ConfigDump` field 7 — the 1-Wire slot types (sticker-firmware#413).
///
/// Runtime state, not a setting: only the autonomous boot dump carries it, `get-config` does
/// not, and nothing can write it.
const W1_SLOT_TYPE_FIELD: u32 = 7;

/// The retained `device_info` document — §2.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfoDocument {
    pub dev_eui: String,
    /// RFC3339, when the `Info` frame was received — the `node-get` cache slot's own `at`.
    pub received_at: String,
    /// The decoded `Info`, exactly the `node-get device-info` slot value. The firmware owns the
    /// shape, so consumers read its keys defensively. `claim_token` is never in it.
    pub device_info: Map<String, Value>,
}

impl DeviceInfoDocument {
    pub fn new(dev_eui: &str, received_at: &str, device_info: Map<String, Value>) -> Self {
        DeviceInfoDocument {
            dev_eui: dev_eui.to_string(),
            received_at: received_at.to_string(),
            device_info,
        }
    }
}

/// The retained `config` document — §2.2. Also the `node-get config` cache slot's value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigDocument {
    pub dev_eui: String,
    /// RFC3339, when the dump's last page was received.
    #[serde(default)]
    pub received_at: String,
    /// The command sequence the dump answers; `0` for the autonomous boot dump.
    #[serde(default)]
    pub seq: u32,
    #[serde(default)]
    pub page_count: u32,
    /// `group.field` → value, for every key the dump carried. A subset, in general: merge it,
    /// do not replace with it.
    #[serde(default)]
    pub config: Map<String, Value>,
    /// Slot 1..4, `empty` | `dallas` | `machine-probe` | `type<N>`. `None` when the dump did not
    /// carry field 7 — which is every dump but the boot one, so `None` is "not reported", not
    /// "no slots".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub w1_slot_type: Option<Vec<String>>,
    /// The reassembled dump, verbatim. Kept so nothing the renderer does not know is lost, and
    /// so a consumer of the old bytes-only shape keeps working.
    #[serde(default)]
    pub body_hex: String,
}

impl ConfigDocument {
    /// Render a reassembled dump. The one constructor, used by the live uplink path and by the
    /// connect-time replay alike, so the two cannot publish different documents for one dump.
    pub fn render(
        dev_eui: &str,
        received_at: &str,
        seq: u32,
        page_count: u32,
        body: &[u8],
    ) -> Self {
        let RenderedDump {
            config,
            w1_slot_type,
        } = render_config_dump(body);
        ConfigDocument {
            dev_eui: dev_eui.to_string(),
            received_at: received_at.to_string(),
            seq,
            page_count,
            config,
            w1_slot_type,
            body_hex: hex(body),
        }
    }
}

impl ConfigDocument {
    /// This (newer) dump merged over the configuration already held — `node_reported_state.md`
    /// §2.2, "The Hub accumulates".
    ///
    /// `config` becomes the union with this dump's values winning, `w1_slot_type` stays the
    /// prior one unless this dump carries its own, and everything else — `seq`, `page_count`,
    /// `received_at`, `body_hex` — keeps describing this dump. A prior in the bytes-only shape a
    /// cache held before the renderer existed is rendered first, so its keys are not lost to
    /// the upgrade.
    pub fn over(mut self, prior: &ConfigDocument) -> Self {
        let held = prior.held();
        let mut config = held.config;
        config.extend(std::mem::take(&mut self.config));
        self.config = config;
        if self.w1_slot_type.is_none() {
            self.w1_slot_type = held.w1_slot_type;
        }
        self
    }

    /// This dump **instead of** the configuration held — a complete answer to a `get-config`
    /// (§2.2, amended 2026-09-23).
    ///
    /// That answer is the Node's whole configuration, and a key missing from it is gone: an
    /// emptied alarm slot sends no field at all, so merging kept eight cleared rules alive on
    /// the bench. Only `w1_slot_type` survives from the prior, because `get-config` never
    /// carries it — it is the boot dump's alone.
    pub fn replacing(mut self, prior: &ConfigDocument) -> Self {
        if self.w1_slot_type.is_none() {
            self.w1_slot_type = prior.held().w1_slot_type;
        }
        self
    }

    /// What a held document says, rendering the bytes-only shape an older cache kept.
    fn held(&self) -> RenderedDump {
        if self.config.is_empty() && !self.body_hex.is_empty() {
            return unhex(&self.body_hex)
                .map(|b| render_config_dump(&b))
                .unwrap_or_default();
        }
        RenderedDump {
            config: self.config.clone(),
            w1_slot_type: self.w1_slot_type.clone(),
        }
    }
}

/// What [`render_config_dump`] reads out of a dump.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RenderedDump {
    pub config: Map<String, Value>,
    pub w1_slot_type: Option<Vec<String>>,
}

/// Render one reassembled `ConfigDump` body (version byte and `Response` envelope already
/// stripped). Never fails: like every decoder here it mirrors the reference's tolerance, and a
/// frame it cannot fully read yields what it could.
///
/// Pages are concatenated before this runs, and a protobuf message concatenated with another
/// is their merge — so a group split across pages comes out whole, and the repeated
/// `page_index`/`page_count` fields are simply ignored.
pub fn render_config_dump(body: &[u8]) -> RenderedDump {
    let mut out = RenderedDump::default();
    let mut pos = 0usize;

    while pos < body.len() {
        let tag = read_tag(body, pos);
        pos = tag.next;

        if tag.wire == 0 {
            let v = read_varint(body, pos);
            pos = v.next;
            // The unpacked form of field 7: one element per tag. Field 1/2 are paging.
            if tag.field == W1_SLOT_TYPE_FIELD {
                out.w1_slot_type
                    .get_or_insert_with(Vec::new)
                    .push(w1_slot_label(v.value));
            }
            continue;
        }
        if tag.wire != 2 {
            match skip(body, tag.wire, pos) {
                Some(next) => pos = next,
                None => break,
            }
            continue;
        }

        let len = read_varint(body, pos);
        pos = len.next;
        let end = (pos + len.value as usize).min(body.len());
        let sub = &body[pos..end];
        pos = end;

        if tag.field == W1_SLOT_TYPE_FIELD {
            let slots = out.w1_slot_type.get_or_insert_with(Vec::new);
            let mut p = 0usize;
            while p < sub.len() {
                let v = read_varint(sub, p);
                p = v.next;
                slots.push(w1_slot_label(v.value));
            }
            continue;
        }
        if let Some(group) = ConfigGroup::ALL
            .into_iter()
            .find(|g| g.config_dump_field() == tag.field)
        {
            render_group(group, sub, &mut out.config);
        }
    }

    out
}

/// One group submessage into `group.field` keys.
fn render_group(group: ConfigGroup, bytes: &[u8], config: &mut Map<String, Value>) {
    let mut pos = 0usize;
    while pos < bytes.len() {
        let tag = read_tag(bytes, pos);
        pos = tag.next;

        let value = match tag.wire {
            0 => {
                let v = read_varint(bytes, pos);
                pos = v.next;
                Wire::Varint(v.value)
            }
            2 => {
                let len = read_varint(bytes, pos);
                pos = len.next;
                let end = (pos + len.value as usize).min(bytes.len());
                let sub = &bytes[pos..end];
                pos = end;
                Wire::Bytes(sub)
            }
            other => {
                match skip(bytes, other, pos) {
                    Some(next) => pos = next,
                    None => break,
                }
                continue;
            }
        };

        let Some(key) = command_encode::key_by_wire(group, tag.field) else {
            continue;
        };
        let rendered = match (key.kind, value) {
            // Never rendered — the §9.7 `claim_token` rule applied to the dump. A retained topic
            // is readable by every subscriber for as long as the broker lives.
            (ValueKind::Secret, _) => None,
            (ValueKind::Bool, Wire::Varint(v)) => Some(Value::Bool(v != 0)),
            (ValueKind::Uint, Wire::Varint(v)) => Some(Value::from(v)),
            (ValueKind::Enum(names), Wire::Varint(v)) => Some(
                names
                    .get(v as usize)
                    .map(|n| Value::from(*n))
                    // A variant this build does not know stays visible as its number.
                    .unwrap_or_else(|| Value::from(v)),
            ),
            (ValueKind::Hex { .. }, Wire::Bytes(b)) => Some(Value::from(hex(b))),
            // A wire type the key cannot have: not guessed at, it stays in `body_hex`.
            _ => None,
        };
        if let Some(v) = rendered {
            config.insert(key.full_name(), v);
        }
    }
}

/// One field's payload, as far as the renderer needs to tell them apart.
#[derive(Clone, Copy)]
enum Wire<'a> {
    Varint(u32),
    Bytes(&'a [u8]),
}

/// The inverse of [`hex`], for a cached `body_hex`. `None` on anything that is not an even run
/// of hex digits — a corrupt cache entry is skipped, not guessed at.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok()))
        .collect()
}

/// A 1-Wire slot type by wire value: the registry's type ids since v2.0.0 (`0 empty, 2 dallas,
/// 3 machine-probe`); anything else renders `type<N>`, the firmware's own `ttn.js` fallback, so
/// a newer probe kind stays visible rather than lost.
fn w1_slot_label(v: u32) -> String {
    crate::decode::sensor_types::w1_type_label(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hexbytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// The `ConfigDump` body of a boot settings-info frame, version byte and the
    /// `Response.config_dump` envelope (`22 len`) stripped.
    fn dump_body(frame_hex: &str) -> Vec<u8> {
        let frame = hexbytes(frame_hex);
        assert_eq!(frame[0], 0x01, "version byte");
        assert_eq!(frame[1], 0x22, "Response field 4, length-delimited");
        frame[3..3 + frame[2] as usize].to_vec()
    }

    /// A v2.0.0 boot settings-info (`app_cmd.c` `cs_app_ids` 2, 3, 4 and `cs_sensor_ids` 1, 2,
    /// 5, 6, 8, 9, 22, 27, 28, 29): built field by field from the firmware's proto at 408eeaf,
    /// every proto3 `optional` on the wire, `cap_sht` true, the three GP fields `none`.
    const BOOT_DUMP_NO_W1: &str = "01222510012207103c18840720002a18\
        080010002800300040014800b00101d80100e00100e80100";
    const BOOT_DUMP_W1: &str = "01222b10012207103c18840720002a18\
        080010002800300040014800b00101d80100e00100e80100\
        3a0400000000";

    #[test]
    fn the_boot_dump_renders_its_application_and_sensor_keys() {
        let r = render_config_dump(&dump_body(BOOT_DUMP_NO_W1));
        assert_eq!(r.config["application.interval_sample"], Value::from(60));
        assert_eq!(r.config["application.interval_report"], Value::from(900));
        assert_eq!(r.config["application.history_enable"], Value::Bool(false));
        for cap in [
            "cap_hall_left",
            "cap_hall_right",
            "cap_light_sensor",
            "cap_barometer",
            "cap_accelerometer",
        ] {
            assert_eq!(
                r.config[&format!("sensors.{cap}")],
                Value::Bool(false),
                "{cap}"
            );
        }
        assert_eq!(r.config["sensors.cap_w1_sensors"], Value::Bool(true));
        assert_eq!(r.config["sensors.cap_sht"], Value::Bool(true));
        for gpio in ["cap_gpio_mode", "cap_gpio_input_a", "cap_gpio_input_b"] {
            assert_eq!(r.config[&format!("sensors.{gpio}")], "none", "{gpio}");
        }
        assert_eq!(
            r.config.len(),
            13,
            "3 application keys + 10 sensor keys: {:?}",
            r.config
        );
        assert_eq!(
            r.w1_slot_type, None,
            "a dump without field 7 reports no slots"
        );
    }

    #[test]
    fn the_boot_dump_with_a_1wire_image_reports_four_empty_slots() {
        let r = render_config_dump(&dump_body(BOOT_DUMP_W1));
        assert_eq!(
            r.w1_slot_type,
            Some(vec!["empty".to_string(); 4]),
            "3a 04 00 00 00 00 is field 7, packed, four zeros"
        );
        assert_eq!(r.config.len(), 13);
    }

    #[test]
    fn slot_types_map_by_value_and_unknown_ones_are_named() {
        // Packed [3, 2, 0, 9]: the v2.0.0 registry ids (#430).
        let r = render_config_dump(&[0x3a, 0x04, 0x03, 0x02, 0x00, 0x09]);
        assert_eq!(
            r.w1_slot_type,
            Some(vec![
                "machine-probe".to_string(),
                "dallas".to_string(),
                "empty".to_string(),
                "type9".to_string(),
            ])
        );
        // The unpacked encoding is the same list.
        let r = render_config_dump(&[0x38, 0x03, 0x38, 0x02]);
        assert_eq!(
            r.w1_slot_type,
            Some(vec!["machine-probe".to_string(), "dallas".to_string()])
        );
        // 1 is the motherboard, never a slot type; v1.4.0's dallas id reads as unknown.
        let r = render_config_dump(&[0x38, 0x01]);
        assert_eq!(r.w1_slot_type, Some(vec!["type1".to_string()]));
    }

    #[test]
    fn key_material_is_never_rendered() {
        // lorawan group (ConfigDump field 3): region = us915 (1), appkey (field 9, 16 bytes),
        // deveui (field 6, 8 bytes).
        let mut lorawan = vec![0x08, 0x01, 0x4a, 0x10];
        lorawan.extend([0xaa; 16]);
        lorawan.extend([0x32, 0x08]);
        lorawan.extend([0x58, 0x76, 0x07, 0x00, 0x00, 0x00, 0x04, 0x13]);
        let mut body = vec![0x1a, lorawan.len() as u8];
        body.extend(&lorawan);

        let r = render_config_dump(&body);
        assert_eq!(r.config["lorawan.region"], Value::from("us915"));
        assert_eq!(r.config["lorawan.deveui"], Value::from("5876070000000413"));
        assert!(
            !r.config.contains_key("lorawan.appkey"),
            "a secret never reaches the rendered map: {:?}",
            r.config
        );
        assert_eq!(r.config.len(), 2);
    }

    #[test]
    fn an_enum_past_its_table_and_an_unknown_field_are_not_guessed_at() {
        // sensors group (field 5): accel_motion_sensitivity (10) = 7, unknown field 30 = 1.
        let body = [0x2a, 0x05, 0x50, 0x07, 0xf0, 0x01, 0x01];
        let r = render_config_dump(&body[..]);
        assert_eq!(
            r.config["sensors.accel_motion_sensitivity"],
            Value::from(7),
            "an out-of-table variant stays visible as its number"
        );
        assert_eq!(r.config.len(), 1, "field 30 is not in the key table");
    }

    #[test]
    fn a_group_split_across_two_pages_renders_whole() {
        // Page 0 carries interval_sample, page 1 interval_report, both in the application group.
        let page0 = [0x08, 0x00, 0x10, 0x02, 0x22, 0x02, 0x10, 0x3c];
        let page1 = [0x08, 0x01, 0x10, 0x02, 0x22, 0x03, 0x18, 0x84, 0x07];
        let joined: Vec<u8> = page0.iter().chain(page1.iter()).copied().collect();
        let r = render_config_dump(&joined);
        assert_eq!(r.config["application.interval_sample"], Value::from(60));
        assert_eq!(r.config["application.interval_report"], Value::from(900));
    }

    /// PS-1 (bench 2026-09-27, STICKER 0413 on 7be011e): the `p2p` group is `ConfigDump` field
    /// 8, split across two pages here — `42 08 08 a0cff89d03 10 07` and `42 02 18 0e`. It was
    /// dropped as an unknown field; it renders like any other group now.
    #[test]
    fn the_p2p_group_renders_even_split_across_pages() {
        let page4 = hexbytes("420808a0cff89d031007");
        let page5 = hexbytes("4202180e");
        let joined: Vec<u8> = page4.iter().chain(page5.iter()).copied().collect();
        let r = render_config_dump(&joined);
        assert_eq!(r.config["p2p.frequency"], Value::from(868_100_000));
        assert_eq!(r.config["p2p.spreading_factor"], Value::from(7));
        assert_eq!(r.config["p2p.tx_power"], Value::from(14));
        assert_eq!(r.config.len(), 3, "{:?}", r.config);
        assert_eq!(r.w1_slot_type, None, "field 8 is not the slot types");
    }

    /// sticker-firmware 3edb4a4 (#460 T2c) dumps alarms field 21, `radio_alarm_ack`, after
    /// `alarm_buzzer_mode` (20). Unknown before, it was dropped from the rendered config.
    #[test]
    fn radio_alarm_ack_renders_from_the_alarms_group() {
        // ConfigDump field 6 (alarms): a0 01 00 = alarm_buzzer_mode off, a8 01 01 = field 21 true.
        let r = render_config_dump(&hexbytes("3206a00100a80101"));
        assert_eq!(r.config["alarms.alarm_buzzer_mode"], Value::from("off"));
        assert_eq!(r.config["alarms.radio_alarm_ack"], Value::Bool(true));
        assert_eq!(r.config.len(), 2, "{:?}", r.config);
    }

    #[test]
    fn garbage_never_panics() {
        for bytes in [
            &[][..],
            &[0xff][..],
            &[0x22, 0x7f, 0x10][..],
            &[0x3a, 0x05, 0x80][..],
            &[0x0b, 0x0c][..],
        ] {
            let _ = render_config_dump(bytes);
        }
    }

    #[test]
    fn the_document_keeps_the_raw_bytes_beside_the_rendering() {
        let body = dump_body(BOOT_DUMP_W1);
        let doc = ConfigDocument::render("5876070000000413", "2026-09-22T19:09:09Z", 0, 1, &body);
        assert_eq!(doc.body_hex, hex(&body));
        assert_eq!(doc.config["application.interval_report"], Value::from(900));

        let json = serde_json::to_value(&doc).expect("serializes");
        assert_eq!(json["received_at"], Value::from("2026-09-22T19:09:09Z"));
        assert_eq!(json["w1_slot_type"][0], Value::from("empty"));
        let back: ConfigDocument = serde_json::from_value(json).expect("round-trips");
        assert_eq!(back, doc);
    }

    fn doc(config: Value, w1: Option<&[&str]>, received_at: &str) -> ConfigDocument {
        ConfigDocument {
            dev_eui: "5876070000000413".into(),
            received_at: received_at.into(),
            seq: 0,
            page_count: 1,
            config: config.as_object().cloned().expect("object"),
            w1_slot_type: w1.map(|v| v.iter().map(|s| s.to_string()).collect()),
            body_hex: String::new(),
        }
    }

    /// The bench case (#409, 2026-09-23): an 8-key `GetParam` answer must not erase what a full
    /// `GetConfig` read — the newer values win, every other key stays.
    #[test]
    fn a_partial_dump_merges_over_what_is_held() {
        let full = doc(
            serde_json::json!({"lorawan.region": "eu868", "application.interval_report": 900,
                               "alarms.alarm_limit": 3}),
            Some(&["dallas", "empty"]),
            "T1",
        );
        let partial = ConfigDocument {
            seq: 2,
            page_count: 2,
            ..doc(
                serde_json::json!({"application.interval_report": 600}),
                None,
                "T2",
            )
        };
        let merged = partial.over(&full);
        assert_eq!(
            merged.config["application.interval_report"], 600,
            "newer wins"
        );
        assert_eq!(merged.config["lorawan.region"], "eu868", "the rest stays");
        assert_eq!(merged.config["alarms.alarm_limit"], 3);
        assert_eq!(merged.config.len(), 3);
        assert_eq!(
            merged.w1_slot_type,
            Some(vec!["dallas".to_string(), "empty".to_string()]),
            "a dump without field 7 keeps the slots"
        );
        assert_eq!(
            (merged.seq, merged.page_count),
            (2, 2),
            "the latest dump's own facts"
        );
        assert_eq!(merged.received_at, "T2");
    }

    /// Bench, 2026-09-23: eight alarm rules cleared on the device stayed in the Hub's document
    /// after a complete `get-config`, because an empty slot sends no field. That answer replaces.
    #[test]
    fn a_complete_get_config_replaces_what_is_held_but_keeps_the_slots() {
        let held = doc(
            serde_json::json!({"alarms.alarm_0": "0300", "alarms.alarm_limit": 3,
                               "application.interval_report": 900}),
            Some(&["dallas"]),
            "T1",
        );
        let full = doc(
            serde_json::json!({"alarms.alarm_limit": 10, "application.interval_report": 900}),
            None,
            "T2",
        );
        let replaced = full.replacing(&held);
        assert!(
            !replaced.config.contains_key("alarms.alarm_0"),
            "the cleared rule is gone"
        );
        assert_eq!(replaced.config["alarms.alarm_limit"], 10);
        assert_eq!(replaced.config.len(), 2);
        assert_eq!(
            replaced.w1_slot_type,
            Some(vec!["dallas".to_string()]),
            "get-config never carries the slots, so they stay"
        );
    }

    #[test]
    fn a_dump_with_slots_replaces_them() {
        let prior = doc(serde_json::json!({}), Some(&["dallas"]), "T1");
        let merged = doc(serde_json::json!({}), Some(&["empty"]), "T2").over(&prior);
        assert_eq!(merged.w1_slot_type, Some(vec!["empty".to_string()]));
    }

    /// A cache from before the renderer holds bytes only; its keys still count as held.
    #[test]
    fn a_bytes_only_prior_is_rendered_before_the_merge() {
        let prior = ConfigDocument {
            body_hex: hex(&dump_body(BOOT_DUMP_W1)),
            ..doc(serde_json::json!({}), None, "T1")
        };
        let merged = doc(serde_json::json!({"lorawan.region": "eu868"}), None, "T2").over(&prior);
        assert_eq!(merged.config["application.interval_report"], 900);
        assert_eq!(merged.config["lorawan.region"], "eu868");
        assert_eq!(merged.w1_slot_type, Some(vec!["empty".to_string(); 4]));
    }

    #[test]
    fn unhex_inverts_hex_and_refuses_garbage() {
        assert_eq!(unhex("00ff3a"), Some(vec![0x00, 0xff, 0x3a]));
        assert_eq!(unhex(""), Some(vec![]));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }

    /// A cache written before the rendering existed holds only `dev_eui`/`seq`/`page_count`/
    /// `body_hex`. It must still deserialize — the replay reads it to re-render.
    #[test]
    fn the_old_bytes_only_shape_still_deserializes() {
        let old = serde_json::json!({
            "dev_eui": "5876070000000413", "seq": 0, "page_count": 1,
            "body_hex": "10012207103c",
        });
        let doc: ConfigDocument = serde_json::from_value(old).expect("old shape");
        assert!(doc.config.is_empty());
        assert_eq!(doc.w1_slot_type, None);
        assert_eq!(doc.body_hex, "10012207103c");
    }
}
