//! Node parameter metadata from sticker-firmware's own registry: a snapshot of its
//! `app/src/app_config.yml` at 0cbb6c6 (hynek/p2p-link-check, decision #22, going into v2.0.0;
//! it superseded the 7be011e snapshot, which had superseded the v2.0.0 177a3dd one, both on
//! 2026-09-27), read as it is rather than copied into a second table (devices.node.md §3.6: the
//! portal's parameter registry comes from the firmware).
//!
//! It carries what the key catalog in [`super::command_encode`] does not: each parameter's
//! range, enum values, default, help text and the transports that may write it. control-radio
//! uses it to refuse a value the firmware would answer `OUT_OF_RANGE` or `NOT_WRITABLE` before
//! the frame costs an RX window; the portal uses it to draw its inputs.
//!
//! Only sticker-firmware v2.0.0 is targeted (nodes_phase1.cs.md Q12). A new snapshot is dropped
//! into `data/`, and [`tests::the_key_catalog_agrees_with_the_firmware_registry`] names every
//! key whose wire number or type moved.
//!
//! 7be011e changed two things read here: the `p2p` group (`p2p_frequency`,
//! `p2p_spreading_factor`, `p2p_tx_power`, all `writable: [shell]`), and the transport name
//! `radio`, which the firmware now writes for "both radios" where it used to write `lrw`.
//! 0cbb6c6 changed one: `p2p_spreading_factor` defaults to 7, the network default the Hub
//! shares (decision #22, `p2p_link_check.md` §3.1).

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::error::Transport;

const APP_CONFIG_YML: &str = include_str!("../../data/app_config.v2.0.0.yml");

/// What values a parameter takes.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamKind {
    Bool,
    /// An integer. `zero_allowed` is the firmware's "0 means off" beside the range
    /// (`interval_sample`: 0, or 5 to 3600).
    Int {
        min: Option<i64>,
        max: Option<i64>,
        zero_allowed: bool,
    },
    /// Wire values with the firmware's shell spelling, in value order.
    Enum(Vec<(String, u32)>),
    Bytes,
}

/// One firmware parameter.
#[derive(Debug, Clone)]
pub struct NodeParam {
    /// `lorawan` | `application` | `sensors` | `alarms` | `p2p` | `root`.
    pub group: String,
    /// The group-local name the key catalog uses (`region`, not the firmware's `lrw_region`).
    pub name: String,
    pub proto_id: Option<u32>,
    pub kind: ParamKind,
    pub default: Option<String>,
    pub help: Option<String>,
    /// Whether a LoRaWAN downlink may write it. The firmware answers `NOT_WRITABLE` otherwise.
    pub writable_over_lorawan: bool,
    /// Whether a P2P downlink may write it, by the same rule.
    pub writable_over_p2p: bool,
    /// The firmware's `writable:` list as written (`shell`, `nfc`, `radio`, …). `None` is every
    /// transport but `plain_text`, the registry's default.
    pub writable: Option<Vec<String>>,
}

impl NodeParam {
    /// `group.name`, the key catalog's spelling.
    pub fn full_name(&self) -> String {
        format!("{}.{}", self.group, self.name)
    }

    /// Whether a downlink on `transport` may write it.
    pub fn writable_over(&self, transport: Transport) -> bool {
        match transport {
            Transport::Lorawan => self.writable_over_lorawan,
            Transport::P2p => self.writable_over_p2p,
        }
    }

    /// Whether both radios may write it. A Node moves between carriers, so what the portal
    /// offers to send over the air has to be writable on either.
    pub fn writable_over_the_air(&self) -> bool {
        self.writable_over_lorawan && self.writable_over_p2p
    }

    /// Whether the firmware's shell is the only way to write it (`writable: [shell]`), as for
    /// the P2P radio tuning.
    pub fn shell_only(&self) -> bool {
        self.writable
            .as_deref()
            .is_some_and(|t| !t.is_empty() && t.iter().all(|t| t == "shell"))
    }

    /// Whether `value` is inside the firmware's range for this parameter. A parameter with no
    /// range takes any value; only an integer has one.
    pub fn accepts(&self, value: i64) -> bool {
        match &self.kind {
            ParamKind::Int {
                min,
                max,
                zero_allowed,
            } => {
                (*zero_allowed && value == 0)
                    || (min.is_none_or(|m| value >= m) && max.is_none_or(|m| value <= m))
            }
            _ => true,
        }
    }

    /// The range as an operator reads it: `60–86400`, `0 or 5–3600`. `None` without a range.
    pub fn range_text(&self) -> Option<String> {
        let ParamKind::Int {
            min,
            max,
            zero_allowed,
        } = &self.kind
        else {
            return None;
        };
        let range = match (min, max) {
            (Some(lo), Some(hi)) => format!("{lo}\u{2013}{hi}"),
            (Some(lo), None) => format!("\u{2265} {lo}"),
            (None, Some(hi)) => format!("\u{2264} {hi}"),
            (None, None) => return None,
        };
        Some(if *zero_allowed {
            format!("0 or {range}")
        } else {
            range
        })
    }
}

#[derive(Deserialize)]
struct RawDoc {
    enums: BTreeMap<String, Vec<RawEnumValue>>,
    proto: RawProto,
    parameters: Vec<RawParam>,
}

#[derive(Deserialize)]
struct RawEnumValue {
    value: u32,
    shell: Option<String>,
    name: String,
}

#[derive(Deserialize)]
struct RawProto {
    groups: Vec<RawGroup>,
}

#[derive(Deserialize)]
struct RawGroup {
    key: String,
    strip_prefix: Option<String>,
}

#[derive(Deserialize)]
struct RawParam {
    name: String,
    /// The proto field name when it is not the shell name (435273c6: `radio_deveui` is
    /// proto `deveui`). The catalog keys by the wire name.
    proto_name: Option<String>,
    proto_group: Option<String>,
    proto_id: Option<u32>,
    #[serde(rename = "type")]
    kind: String,
    min: Option<i64>,
    max: Option<i64>,
    #[serde(rename = "enum")]
    enum_name: Option<String>,
    default: Option<serde_yaml::Value>,
    help: Option<String>,
    writable: Option<Vec<String>>,
    extras: Option<RawExtras>,
}

#[derive(Deserialize)]
struct RawExtras {
    zero_allowed: Option<bool>,
}

fn parse(yaml: &str) -> Vec<NodeParam> {
    let doc: RawDoc = match serde_yaml::from_str(yaml) {
        Ok(doc) => doc,
        // A snapshot that does not parse is a build defect, caught by the tests below; at run
        // time the catalog then carries no ranges rather than taking the process down.
        Err(_) => return Vec::new(),
    };
    let prefixes: BTreeMap<String, String> = doc
        .proto
        .groups
        .into_iter()
        .filter_map(|g| g.strip_prefix.map(|p| (g.key, p)))
        .collect();
    doc.parameters
        .into_iter()
        .map(|p| {
            let group = p.proto_group.unwrap_or_else(|| "root".to_string());
            let name = match p.proto_name {
                Some(proto_name) => proto_name,
                None => prefixes
                    .get(&group)
                    .and_then(|prefix| p.name.strip_prefix(prefix.as_str()))
                    .map(str::to_string)
                    .unwrap_or(p.name),
            };
            let kind = match p.kind.as_str() {
                "bool" => ParamKind::Bool,
                "enum" => ParamKind::Enum(
                    p.enum_name
                        .as_deref()
                        .and_then(|e| doc.enums.get(e))
                        .map(|values| {
                            values
                                .iter()
                                .map(|v| {
                                    let spelled =
                                        v.shell.clone().unwrap_or_else(|| v.name.to_lowercase());
                                    (spelled, v.value)
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                ),
                "bytes" => ParamKind::Bytes,
                _ => ParamKind::Int {
                    min: p.min,
                    max: p.max,
                    zero_allowed: p.extras.and_then(|e| e.zero_allowed).unwrap_or(false),
                },
            };
            NodeParam {
                group,
                name,
                proto_id: p.proto_id,
                kind,
                default: p.default.and_then(|d| match d {
                    serde_yaml::Value::String(s) => Some(s),
                    serde_yaml::Value::Number(n) => Some(n.to_string()),
                    serde_yaml::Value::Bool(b) => Some(b.to_string()),
                    _ => None,
                }),
                help: p.help,
                writable_over_lorawan: writable_by(p.writable.as_deref(), "lrw"),
                writable_over_p2p: writable_by(p.writable.as_deref(), "p2p"),
                writable: p.writable,
            }
        })
        .collect()
}

/// Whether a `writable:` list lets the one radio `radio` (`lrw` | `p2p`) write. No list is
/// every transport but `plain_text`, the registry's default; `radio` is both radios (7be011e's
/// `transports:` note: "`lrw` / `p2p` alone only for a one-radio command").
fn writable_by(list: Option<&[String]>, radio: &str) -> bool {
    list.is_none_or(|t| t.iter().any(|t| t == radio || t == "radio"))
}

/// Every parameter of the firmware registry.
pub fn node_params() -> &'static [NodeParam] {
    static PARAMS: OnceLock<Vec<NodeParam>> = OnceLock::new();
    PARAMS.get_or_init(|| parse(APP_CONFIG_YML))
}

/// One parameter by its catalog name (`application.interval_report`).
pub fn node_param(full_name: &str) -> Option<&'static NodeParam> {
    let (group, name) = full_name.split_once('.')?;
    node_params()
        .iter()
        .find(|p| p.group == group && p.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::command_encode::{ValueKind, CONFIG_KEYS};

    #[test]
    fn the_snapshot_parses_with_the_ranges_the_firmware_enforces() {
        assert!(node_params().len() > 50, "the whole registry");
        let report = node_param("application.interval_report").expect("interval_report");
        assert!(report.accepts(60) && report.accepts(86_400));
        assert!(!report.accepts(59) && !report.accepts(86_401));
        assert_eq!(report.range_text().as_deref(), Some("60\u{2013}86400"));

        let sample = node_param("application.interval_sample").expect("interval_sample");
        assert!(sample.accepts(0), "0 = sample on report");
        assert!(!sample.accepts(4) && sample.accepts(5) && !sample.accepts(3601));
        assert_eq!(sample.range_text().as_deref(), Some("0 or 5\u{2013}3600"));

        assert!(node_param("lorawan.region").is_some(), "lrw_ is stripped");
        assert!(
            !node_param("lorawan.adr")
                .expect("adr")
                .writable_over_lorawan
        );
        assert!(
            !node_param("application.vendor_reset_allow")
                .expect("vendor_reset_allow")
                .writable_over_lorawan
        );
        assert!(
            node_param("alarms.alarm_0")
                .expect("alarm_0")
                .writable_over_lorawan
        );
        assert!(
            node_param("sensors.cap_barometer")
                .expect("cap_barometer")
                .writable_over_lorawan
        );
    }

    /// 7be011e writes `radio` for both radios where 177a3dd wrote `lrw`: an alarm slot is
    /// `writable: [nfc, radio]`, and reading `radio` as "not `lrw`" would lock the Hub out of
    /// the alarm table it owns.
    #[test]
    fn radio_means_both_radios() {
        let slot = node_param("alarms.alarm_0").expect("alarm_0");
        assert!(slot.writable_over(Transport::Lorawan) && slot.writable_over(Transport::P2p));
        assert!(slot.writable_over_the_air() && !slot.shell_only());
        let adr = node_param("lorawan.adr").expect("adr");
        assert!(!adr.writable_over(Transport::Lorawan) && !adr.writable_over(Transport::P2p));
        assert!(!adr.shell_only(), "NFC writes it too");
    }

    /// The `p2p` group (7be011e): radio tuning, the prefix stripped like `lrw_`, shell only. The
    /// spreading factor defaults to 7 since 0cbb6c6 (decision #22), the Hub's default too.
    #[test]
    fn the_p2p_group_is_radio_tuning_the_shell_alone_writes() {
        for (name, id, lo, hi, default) in [
            // v2.0.0 (3e08006) moved the default back to 869.525 MHz (h1.6, 10 % duty) and
            // capped the TX power at 14 dBm (RFO_LP path only, #486 W8).
            ("p2p.frequency", 1, 863_000_000, 870_000_000, "869525000"),
            ("p2p.spreading_factor", 2, 6, 12, "7"),
            ("p2p.tx_power", 3, 2, 14, "14"),
        ] {
            let p = node_param(name).unwrap_or_else(|| panic!("{name} in the registry"));
            assert_eq!(p.proto_id, Some(id), "{name}");
            assert_eq!(
                p.kind,
                ParamKind::Int {
                    min: Some(lo),
                    max: Some(hi),
                    zero_allowed: false
                },
                "{name}"
            );
            assert_eq!(p.default.as_deref(), Some(default), "{name}");
            assert!(p.shell_only(), "{name}: writable: [shell]");
            assert!(
                !p.writable_over(Transport::Lorawan) && !p.writable_over(Transport::P2p),
                "{name}: never over a downlink"
            );
        }
    }

    /// 3edb4a4 (#460 T2c): alarm frames confirmed on both radios, a bool that defaults to false
    /// (alarms go unconfirmed) and that a downlink on either radio may write, vendor excluded.
    #[test]
    fn radio_alarm_ack_is_a_bool_both_radios_write() {
        let p = node_param("alarms.radio_alarm_ack").expect("in the registry");
        assert_eq!(p.proto_id, Some(21));
        assert_eq!(p.kind, ParamKind::Bool);
        assert_eq!(p.default.as_deref(), Some("false"));
        assert!(p.writable_over(Transport::Lorawan) && p.writable_over(Transport::P2p));
        assert!(p.writable_over_the_air() && !p.shell_only());
    }

    /// Fields 13/14 were renamed in 7be011e (`lrw_link_check_*` → `radio_link_check_*`); no
    /// `lrw_` prefix to strip, so the whole name stays, as the firmware's `ttn.js` spells it.
    #[test]
    fn the_link_check_keys_carry_the_radio_names() {
        let interval = node_param("lorawan.radio_link_check_interval").expect("renamed key");
        assert_eq!(interval.proto_id, Some(13));
        let rejoin = node_param("lorawan.radio_link_check_fail_rejoin").expect("renamed key");
        assert_eq!(rejoin.proto_id, Some(14));
        assert!(node_param("lorawan.link_check_interval").is_none());
    }

    /// The key catalog carries the wire numbers; the firmware registry is where they come
    /// from. A key whose group, field number or type differs from the snapshot would be sent
    /// to the wrong field.
    #[test]
    fn the_key_catalog_agrees_with_the_firmware_registry() {
        let mut problems = Vec::new();
        for key in CONFIG_KEYS {
            let full = key.full_name();
            let Some(param) = node_param(&full) else {
                problems.push(format!("{full}: not in the firmware registry"));
                continue;
            };
            if param.proto_id != Some(key.field) {
                problems.push(format!(
                    "{full}: field {} here, {:?} in the firmware",
                    key.field, param.proto_id
                ));
            }
            let agrees = matches!(
                (key.kind, &param.kind),
                (ValueKind::Bool, ParamKind::Bool)
                    | (ValueKind::Uint, ParamKind::Int { .. })
                    | (ValueKind::Enum(_), ParamKind::Enum(_))
                    | (ValueKind::Hex { .. }, ParamKind::Bytes)
                    | (ValueKind::Secret, ParamKind::Bytes)
            );
            if !agrees {
                problems.push(format!(
                    "{full}: {:?} here, {:?} in the firmware",
                    key.kind, param.kind
                ));
            }
        }
        assert!(problems.is_empty(), "{problems:#?}");
    }

    /// An enum's names are wire values too: a variant the snapshot has and the key catalog
    /// lacks renders as a bare number (`lorawan.region` 3 before `as923` was added here).
    #[test]
    fn the_catalog_enums_spell_every_firmware_variant() {
        let mut problems = Vec::new();
        for key in CONFIG_KEYS {
            let (ValueKind::Enum(names), Some(param)) = (key.kind, node_param(&key.full_name()))
            else {
                continue;
            };
            let ParamKind::Enum(values) = &param.kind else {
                continue;
            };
            for (spelled, value) in values {
                if names.get(*value as usize) != Some(&spelled.as_str()) {
                    problems.push(format!(
                        "{}: firmware {value} = {spelled:?}, catalog {:?}",
                        key.full_name(),
                        names.get(*value as usize)
                    ));
                }
            }
        }
        assert!(problems.is_empty(), "{problems:#?}");
    }
}
