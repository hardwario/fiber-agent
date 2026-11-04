//! The fPort-85 **downlink** encoder — `node-firmware-v1.4.0.md` §6 (command vocabulary) and
//! §7 (the config key space), behind `plan.md` §5's `node-send` structured forms.
//!
//! The mirror image of [`crate::decode::command`], which reads the *responses*. Together they
//! are §9.7's command engine: this half turns `get-config` / `get-info` /
//! `set-config <key=value>` / `cmd <name>` into `Command` protobuf bytes, and the other half
//! reads what comes back.
//!
//! # Downlinks carry no version byte
//!
//! fPort-2/3/85 **uplinks** are prefixed with `APP_PROTO_VERSION = 0x01`
//! ([`crate::decode::APP_PROTO_VERSION`]); downlinks are not. Confirmed live on the bench
//! (2026-08-25, `tests/integration/README.md`): every byte string in the table below was
//! produced by the firmware's own `ttn.js encodeDownlink` and accepted by a real Node —
//! `get_info seq 7` is `08072200`, four bytes, and the first is the `seq` field tag. A version
//! byte here would be parsed as field 0, which is not a legal protobuf tag.
//!
//! | Command | seq | Bytes |
//! |---|---|---|
//! | `get_info` | 7 | `08072200` |
//! | `get_config` page 0 | 8 | `08082a00` |
//! | `get_config` page 1 | 9 | `08092a020801` |
//! | `get_settings` | 7 | `0807fa0100` (sticker-firmware #429's `ttn.js` vector, HW 2026-09-23) |
//! | `clock_sync` (network re-sync) | 11 | `080b6200` |
//! | `w1_scan` | 12 | `080c7200` |
//! | `force_send` | 14 | `080e4a00` |
//! | `device_reset` | 13 | `080d4200` — **refused here**, see below |
//!
//! The `device_reset` row is the one the bench sent *by hand* and got
//! `error { code: 3, "transport not allowed" }` back: the firmware's own allow-list
//! (`app/src/app_config.yml`, `transports: [nfc, shell]`) refuses it over the air. This
//! encoder therefore cannot produce it at all — [`CommandTag::policy`] calls it `NfcOnly`,
//! [`Downlink::encode`] re-checks, and no CLI spelling exists. Same for `factory_reset` (23),
//! `set_secret_key` (24) and `req_history_page` (13).
//!
//! # Where the sub-message field numbers come from
//!
//! §6 pins the `Command` oneof tags and §7 pins the four config groups' field numbers, which
//! is everything the uplink decoder needed. Encoding needs two things §6/§7 do not state: the
//! layout of `Command.SetParam` and of the small argument messages (`GetConfig{page}`,
//! `ResetCounters{…}`, `ClockSync{unix_time}`, `ReqHistory{from,to}`). Those come from the
//! firmware's own `app/src/app_config.proto`, which `node-firmware-v1.4.0.md`'s normativity
//! note names as a permitted implementation input alongside `ttn.js` — the copy in this tree
//! is `crates/control-core/proto/app_config.proto`, imported verbatim from the firmware repo.
//! It agrees with §5/§7 everywhere the two overlap (`Response.ConfigDump` = `1 page_index,
//! 2 page_count, 3 lorawan, 4 application, 5 sensors, 6 alarms`, `Error.fault_field =
//! group×100 + tag`), which is what makes it trustworthy for the parts §6/§7 leave out:
//!
//! ```text
//! Command.SetParam { 1 lorawan, 2 application, 3 save (bool), 4 sensors, 5 alarms }
//! ```
//!
//! Note the group numbering differs per message — `1/2/4/5` inside `SetParam`, `3/4/5/6`
//! inside `ConfigDump`, `1/2/3/4` in `fault_field` — so [`ConfigGroup`] carries all three
//! rather than one number used three ways. ⚠️ Doc-amendment flag: §6 should carry the
//! `SetParam` layout, since it is wire contract that an encoder cannot be written without.
//!
//! *Amended 2026-09-27 (sticker-firmware 7be011e, the proto refreshed from it):* a fifth
//! group, `p2p`, is `SetParam` 7, `ConfigDump` 8, `GetParam.p2p_field` 6 and `fault_field`
//! group 5 — the one group whose `GetParam` field is not its fault group, because `GetParam`
//! 5 is `page`. The firmware writes it over the shell only, so this table carries it for the
//! dump renderer and `get-param`; `set-config` refuses it on both radios.
//!
//! # One vocabulary, both directions
//!
//! WP-D cached a `ConfigDump` as its verbatim page bytes and flagged the open question: does
//! the named-key vocabulary that arrives with *this* encoder also teach the cache to render
//! them? It was left for the consumer change, and that change is `node_reported_state.md`:
//! [`crate::decode::snapshot::render_config_dump`] turns a reassembled dump into
//! `{"application.interval_report": 900}` through [`key_by_wire`], the reverse lookup of this
//! very table. [`CONFIG_KEYS`] stays the one list of names, types and wire kinds — the
//! renderer reads what this encoder writes, and a second table could only disagree with it.

use crate::cli::{CounterName, NodeSendCmdArgs, NodeSendKind};
use crate::decode::alarm_rule;
use crate::decode::command::{
    check_clock_sync_epoch, check_radio_policy, check_reset_counters, CommandTag, CounterSelection,
    SeqAllocator,
};
use crate::error::{Component, Error, Kind, Result, Transport};

/// Every fPort-85 exchange rides this port, in both directions (§9.3).
///
/// Deliberately not a `node-send` flag: the structured forms are fPort 85 *by protocol*, and a
/// port flag there would only offer a way to send them somewhere they cannot be understood.
/// `--hex` keeps its required `--fport` for exactly the opposite reason.
pub const COMMAND_FPORT: u8 = 85;

// ---------------------------------------------------------------------------
// protobuf writing
// ---------------------------------------------------------------------------

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// A field tag. `field` above 15 needs two bytes — `lrw_reset` (16) and `lrw_join` (17) are
/// exactly that case, which is why this goes through the varint writer rather than a cast.
fn put_tag(out: &mut Vec<u8>, field: u32, wire: u32) {
    put_varint(out, u64::from((field << 3) | wire));
}

fn put_uint(out: &mut Vec<u8>, field: u32, value: u64) {
    put_tag(out, field, 0);
    put_varint(out, value);
}

fn put_bool(out: &mut Vec<u8>, field: u32, value: bool) {
    put_uint(out, field, u64::from(value));
}

fn put_bytes(out: &mut Vec<u8>, field: u32, body: &[u8]) {
    put_tag(out, field, 2);
    put_varint(out, body.len() as u64);
    out.extend_from_slice(body);
}

// ---------------------------------------------------------------------------
// §7 — the config key space
// ---------------------------------------------------------------------------

/// One of §7's config groups: the four of v1.4.0 and `p2p` (sticker-firmware 7be011e).
///
/// Carries all of its wire identities, because they genuinely differ per message and picking
/// one to reuse would put a config value in the wrong group with no error anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigGroup {
    Lorawan,
    Application,
    Sensors,
    Alarms,
    /// The P2P radio tuning (`AppConfigMessage.P2P`). Written over the shell only.
    P2p,
}

impl ConfigGroup {
    pub const fn as_str(self) -> &'static str {
        match self {
            ConfigGroup::Lorawan => "lorawan",
            ConfigGroup::Application => "application",
            ConfigGroup::Sensors => "sensors",
            ConfigGroup::Alarms => "alarms",
            ConfigGroup::P2p => "p2p",
        }
    }

    /// The `group` half of §5's `Error.fault_field = group×100 + tag`.
    pub const fn fault_group(self) -> u32 {
        match self {
            ConfigGroup::Lorawan => 1,
            ConfigGroup::Application => 2,
            ConfigGroup::Sensors => 3,
            ConfigGroup::Alarms => 4,
            ConfigGroup::P2p => 5,
        }
    }

    /// This group's field number inside `Command.SetParam` (`3` is `save` and `6`
    /// `alarms_replace`, hence the gaps).
    pub const fn set_param_field(self) -> u32 {
        match self {
            ConfigGroup::Lorawan => 1,
            ConfigGroup::Application => 2,
            ConfigGroup::Sensors => 4,
            ConfigGroup::Alarms => 5,
            ConfigGroup::P2p => 7,
        }
    }

    /// This group's `repeated uint32 <group>_field` inside `Command.GetParam`. The fault group
    /// for the first four; `p2p_field` is 6, because 5 is `page`.
    pub const fn get_param_field(self) -> u32 {
        match self {
            ConfigGroup::Lorawan => 1,
            ConfigGroup::Application => 2,
            ConfigGroup::Sensors => 3,
            ConfigGroup::Alarms => 4,
            ConfigGroup::P2p => 6,
        }
    }

    /// This group's field number inside `Response.ConfigDump` (§5) — the reverse direction's
    /// half of the shared vocabulary. `7` is `w1_slot_type`, hence the gap before `p2p`.
    pub const fn config_dump_field(self) -> u32 {
        match self {
            ConfigGroup::Lorawan => 3,
            ConfigGroup::Application => 4,
            ConfigGroup::Sensors => 5,
            ConfigGroup::Alarms => 6,
            ConfigGroup::P2p => 8,
        }
    }

    /// One group name back into a group. Not `FromStr` — see [`crate::northbound::Area`]'s
    /// note; this is half of a `group.field` key, never a standalone parse.
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "lorawan" => Some(ConfigGroup::Lorawan),
            "application" => Some(ConfigGroup::Application),
            "sensors" => Some(ConfigGroup::Sensors),
            "alarms" => Some(ConfigGroup::Alarms),
            "p2p" => Some(ConfigGroup::P2p),
            _ => None,
        }
    }

    pub const ALL: [ConfigGroup; 5] = [
        ConfigGroup::Lorawan,
        ConfigGroup::Application,
        ConfigGroup::Sensors,
        ConfigGroup::Alarms,
        ConfigGroup::P2p,
    ];
}

const REGIONS: &[&str] = &["eu868", "us915", "au915", "as923"];
const NETWORKS: &[&str] = &["public", "private"];
const ACTIVATIONS: &[&str] = &["otaa", "abp"];
const RADIO_MODES: &[&str] = &["off", "lorawan", "p2p"];
const MOTION_SENSITIVITY: &[&str] = &["off", "low", "medium", "high"];
/// `sensors.cap_gpio_mode` / `cap_gpio_input_a/_b` (v2.0.0, #498), shell spellings in value
/// order (`ttn.js` `_SEN_ENUMS`; the proto's `GpioInput.DISABLED` is `none`).
const GPIO_MODES: &[&str] = &["none", "inputs", "pir", "buzzer"];
const GPIO_INPUTS: &[&str] = &["none", "digital", "analog", "pulse"];
/// `p2p.modulation` (v2.0.0, #490).
const P2P_MODULATIONS: &[&str] = &["lora", "fsk"];
const ALARM_BUZZER_MODES: &[&str] = &[
    "off",
    "once",
    "slow",
    "normal",
    "fast",
    "continuous",
    "reserved6",
    "reserved7",
];
const DATARATES: &[&str] = &[
    "auto", "dr0", "dr1", "dr2", "dr3", "dr4", "dr5", "dr6", "dr7",
];

/// How a `key=value` value reaches the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// `true`/`false` (or `1`/`0`) → varint.
    Bool,
    /// An unsigned integer → varint.
    Uint,
    /// A named enum; the index is the wire value. The numeric form is accepted too, because a
    /// firmware that grows a variant this build does not know must stay reachable.
    Enum(&'static [&'static str]),
    /// A fixed-length hex string → length-delimited bytes.
    Hex { bytes: usize },
    /// Settable on the wire, but **never from this CLI**: it is key material, and
    /// `system/design.md` §10 / ledger #22 keep key material out of argv entirely.
    Secret,
}

/// One settable key — §7's `group.field` vocabulary, in both directions.
#[derive(Debug, Clone, Copy)]
pub struct ConfigKey {
    pub group: ConfigGroup,
    /// The field number inside the group sub-message. Identical in `SetParam` and in a
    /// `ConfigDump` page, which is what lets one table serve both directions.
    pub field: u32,
    /// The name after the dot, as `node-firmware-v1.4.0.md` §7 spells it.
    pub name: &'static str,
    pub kind: ValueKind,
}

impl ConfigKey {
    /// `group.field`, the canonical spelling.
    pub fn full_name(&self) -> String {
        format!("{}.{}", self.group.as_str(), self.name)
    }

    /// §5's `fault_field` encoding, so an `Error` response can be matched back to the key that
    /// caused it.
    pub const fn fault_field(&self) -> u32 {
        self.group.fault_group() * 100 + self.field
    }
}

/// §7's key space, as sticker-firmware v2.0.0 has it.
///
/// Ordered by group and field number. The firmware's own registry (`app_config.yml`, snapshot
/// in `data/`, read by [`super::node_params`]) is the source: a test checks every key here
/// against it. *Amended 2026-09-24 (nodes_phase1.cs.md Q12, v2.0.0 only):*
/// `alarms.alarm_notif_time` (2) and `alarms.alarm_light_confirm_delay` (19) are gone, both
/// `reserved` in v2.0.0; `lorawan.datarate` (16) and `alarms.alarm_buzzer_mode` (20) are added.
/// *Amended 2026-09-27 (sticker-firmware 7be011e):* lorawan 13/14 are
/// `radio_link_check_interval` / `radio_link_check_fail_rejoin` (the old names stay as
/// aliases for one release), and the `p2p` group adds `frequency`, `spreading_factor` and
/// `tx_power`. *Amended 2026-09-27 (sticker-firmware 3edb4a4, #460 T2c):* `alarms` adds
/// `radio_alarm_ack` (21), alarm frames confirmed on both radios (default false).
/// *Amended 2026-10-09 (sticker-firmware 435273c6, #430, `node-firmware-v2.0.0.md` §2):*
/// `application.history_sensors` (5) is gone, replaced by `history_channels` (9, 24 B);
/// `application.interval_announce` (8), `sensors.cap_sht` (22) and `sensors.sensor1..4_type`
/// (23..26) are added; `alarms.alarm_N` grows from 17 to 18 bytes.
/// *Amended 2026-10-14 (sticker-firmware v2.0.0 408eeaf, #498, `node-firmware-v2.0.0.md`
/// §13):* `sensors` 3, 4, 7, 19 (`cap_input_a/_b`, `cap_pir_detector`, `cap_buzzer`) are
/// gone, replaced by `cap_gpio_mode` (27) and `cap_gpio_input_a/_b` (28/29); `p2p.modulation`
/// (4) is added; the lorawan aliases are dropped.
pub const CONFIG_KEYS: &[ConfigKey] = &[
    // --- lorawan (group 1) ---
    key(ConfigGroup::Lorawan, 1, "region", ValueKind::Enum(REGIONS)),
    key(ConfigGroup::Lorawan, 2, "sub_band", ValueKind::Uint),
    key(
        ConfigGroup::Lorawan,
        3,
        "network",
        ValueKind::Enum(NETWORKS),
    ),
    key(ConfigGroup::Lorawan, 4, "adr", ValueKind::Bool),
    key(
        ConfigGroup::Lorawan,
        5,
        "activation",
        ValueKind::Enum(ACTIVATIONS),
    ),
    key(
        ConfigGroup::Lorawan,
        6,
        "deveui",
        ValueKind::Hex { bytes: 8 },
    ),
    key(
        ConfigGroup::Lorawan,
        7,
        "joineui",
        ValueKind::Hex { bytes: 8 },
    ),
    key(ConfigGroup::Lorawan, 8, "nwkkey", ValueKind::Secret),
    key(ConfigGroup::Lorawan, 9, "appkey", ValueKind::Secret),
    key(
        ConfigGroup::Lorawan,
        10,
        "devaddr",
        ValueKind::Hex { bytes: 4 },
    ),
    key(ConfigGroup::Lorawan, 11, "nwkskey", ValueKind::Secret),
    key(ConfigGroup::Lorawan, 12, "appskey", ValueKind::Secret),
    // 7be011e renamed these (carrier-neutral link supervision, decision #22); same fields.
    // The old spellings were aliases for one release, gone with v2.0.0 (§13.3).
    key(
        ConfigGroup::Lorawan,
        13,
        "radio_link_check_interval",
        ValueKind::Uint,
    ),
    key(
        ConfigGroup::Lorawan,
        14,
        "radio_link_check_fail_rejoin",
        ValueKind::Uint,
    ),
    key(
        ConfigGroup::Lorawan,
        15,
        "radio_mode",
        ValueKind::Enum(RADIO_MODES),
    ),
    key(
        ConfigGroup::Lorawan,
        16,
        "datarate",
        ValueKind::Enum(DATARATES),
    ),
    // --- application (group 2) ---
    key(ConfigGroup::Application, 1, "calibration", ValueKind::Bool),
    key(
        ConfigGroup::Application,
        2,
        "interval_sample",
        ValueKind::Uint,
    ),
    key(
        ConfigGroup::Application,
        3,
        "interval_report",
        ValueKind::Uint,
    ),
    key(
        ConfigGroup::Application,
        4,
        "history_enable",
        ValueKind::Bool,
    ),
    key(
        ConfigGroup::Application,
        6,
        "battery_level",
        ValueKind::Uint,
    ),
    key(
        ConfigGroup::Application,
        7,
        "vendor_reset_allow",
        ValueKind::Bool,
    ),
    key(
        ConfigGroup::Application,
        8,
        "interval_announce",
        ValueKind::Uint,
    ),
    key(
        ConfigGroup::Application,
        9,
        "history_channels",
        ValueKind::Hex { bytes: 24 },
    ),
    // --- sensors (group 3) ---
    key(ConfigGroup::Sensors, 1, "cap_hall_left", ValueKind::Bool),
    key(ConfigGroup::Sensors, 2, "cap_hall_right", ValueKind::Bool),
    key(ConfigGroup::Sensors, 5, "cap_light_sensor", ValueKind::Bool),
    key(ConfigGroup::Sensors, 6, "cap_barometer", ValueKind::Bool),
    key(ConfigGroup::Sensors, 8, "cap_w1_sensors", ValueKind::Bool),
    key(
        ConfigGroup::Sensors,
        9,
        "cap_accelerometer",
        ValueKind::Bool,
    ),
    key(
        ConfigGroup::Sensors,
        10,
        "accel_motion_sensitivity",
        ValueKind::Enum(MOTION_SENSITIVITY),
    ),
    key(
        ConfigGroup::Sensors,
        11,
        "sensor1_rom",
        ValueKind::Hex { bytes: 8 },
    ),
    key(
        ConfigGroup::Sensors,
        12,
        "sensor2_rom",
        ValueKind::Hex { bytes: 8 },
    ),
    key(
        ConfigGroup::Sensors,
        13,
        "sensor3_rom",
        ValueKind::Hex { bytes: 8 },
    ),
    key(
        ConfigGroup::Sensors,
        14,
        "sensor4_rom",
        ValueKind::Hex { bytes: 8 },
    ),
    key(
        ConfigGroup::Sensors,
        15,
        "hall_left_counter",
        ValueKind::Bool,
    ),
    key(
        ConfigGroup::Sensors,
        16,
        "hall_right_counter",
        ValueKind::Bool,
    ),
    key(ConfigGroup::Sensors, 17, "input_a_counter", ValueKind::Bool),
    key(ConfigGroup::Sensors, 18, "input_b_counter", ValueKind::Bool),
    key(ConfigGroup::Sensors, 22, "cap_sht", ValueKind::Bool),
    key(ConfigGroup::Sensors, 23, "sensor1_type", ValueKind::Uint),
    key(ConfigGroup::Sensors, 24, "sensor2_type", ValueKind::Uint),
    key(ConfigGroup::Sensors, 25, "sensor3_type", ValueKind::Uint),
    key(ConfigGroup::Sensors, 26, "sensor4_type", ValueKind::Uint),
    key(
        ConfigGroup::Sensors,
        27,
        "cap_gpio_mode",
        ValueKind::Enum(GPIO_MODES),
    ),
    key(
        ConfigGroup::Sensors,
        28,
        "cap_gpio_input_a",
        ValueKind::Enum(GPIO_INPUTS),
    ),
    key(
        ConfigGroup::Sensors,
        29,
        "cap_gpio_input_b",
        ValueKind::Enum(GPIO_INPUTS),
    ),
    // --- alarms (group 4) ---
    key(ConfigGroup::Alarms, 1, "alarm_limit", ValueKind::Uint),
    key(
        ConfigGroup::Alarms,
        3,
        "alarm_0",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        4,
        "alarm_1",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        5,
        "alarm_2",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        6,
        "alarm_3",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        7,
        "alarm_4",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        8,
        "alarm_5",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        9,
        "alarm_6",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        10,
        "alarm_7",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        11,
        "alarm_8",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        12,
        "alarm_9",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        13,
        "alarm_10",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        14,
        "alarm_11",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        15,
        "alarm_12",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        16,
        "alarm_13",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        17,
        "alarm_14",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        18,
        "alarm_15",
        ValueKind::Hex {
            bytes: ALARM_SLOT_BYTES,
        },
    ),
    key(
        ConfigGroup::Alarms,
        20,
        "alarm_buzzer_mode",
        ValueKind::Enum(ALARM_BUZZER_MODES),
    ),
    key(ConfigGroup::Alarms, 21, "radio_alarm_ack", ValueKind::Bool),
    // --- p2p (group 5, sticker-firmware 7be011e) --- radio tuning, `writable: [shell]`: read
    // from a dump, never written over the air (node_params refuses it on both radios).
    key(ConfigGroup::P2p, 1, "frequency", ValueKind::Uint),
    key(ConfigGroup::P2p, 2, "spreading_factor", ValueKind::Uint),
    key(ConfigGroup::P2p, 3, "tx_power", ValueKind::Uint),
    key(
        ConfigGroup::P2p,
        4,
        "modulation",
        ValueKind::Enum(P2P_MODULATIONS),
    ),
];

const fn key(group: ConfigGroup, field: u32, name: &'static str, kind: ValueKind) -> ConfigKey {
    ConfigKey {
        group,
        field,
        name,
        kind,
    }
}

/// Look one key up by its `group.field` spelling.
pub fn lookup(name: &str) -> Option<&'static ConfigKey> {
    let (group, field) = name.split_once('.')?;
    let group = ConfigGroup::from_name(group)?;
    CONFIG_KEYS
        .iter()
        .find(|k| k.group == group && k.name == field)
}

/// The reverse direction: which key a `(group, field)` pair on the wire is.
///
/// The half of the shared vocabulary the `ConfigDump` renderer
/// ([`crate::decode::snapshot::render_config_dump`]) reads, so it cannot grow a second,
/// drifting name table (see the module note).
pub fn key_by_wire(group: ConfigGroup, field: u32) -> Option<&'static ConfigKey> {
    CONFIG_KEYS
        .iter()
        .find(|k| k.group == group && k.field == field)
}

/// One parsed `group.field=value`, already encoded as the field it will occupy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub group: ConfigGroup,
    /// `group.field`, canonicalized (an alias resolves to the §7 spelling).
    pub name: String,
    /// The encoded field — tag included — ready to drop into the group sub-message.
    field_bytes: Vec<u8>,
}

fn usage(transport: Transport, message: impl Into<String>) -> Error {
    Error::new(
        Kind::InvalidArgument,
        transport,
        Component::ControlSocket,
        message.into(),
    )
}

fn parse_hex(s: &str) -> std::result::Result<Vec<u8>, String> {
    if !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("not hexadecimal (digits 0-9a-f only, no separators)".to_string());
    }
    if !s.len().is_multiple_of(2) {
        return Err(format!(
            "has an odd number of digits ({}), so its last byte is incomplete",
            s.len()
        ));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            s.get(i..i + 2)
                .and_then(|b| u8::from_str_radix(b, 16).ok())
                .ok_or_else(|| "not hexadecimal".to_string())
        })
        .collect()
}

/// Parse one `group.field=value` assignment.
///
/// Every refusal here happens **before** anything is queued: the wrong value on the air costs
/// an RX window and a duty-cycle slot, and on a Class-A link the operator finds out minutes
/// later, if at all.
pub fn parse_assignment(spec: &str, transport: Transport) -> Result<Assignment> {
    let (key, _) = resolve_assignment(spec, transport)?;

    // What the firmware itself would refuse is refused here, before the frame costs an RX
    // window: a key its registry does not let this radio write (`NOT_WRITABLE`), and in the
    // encode, a number outside its range (`OUT_OF_RANGE`). Key material keeps its own refusal.
    // Both radios are checked: 7be011e's registry names them one by one (`radio` = both), and
    // its `p2p` group is the shell's alone on either (#118 phase 1).
    if !matches!(key.kind, ValueKind::Secret) {
        if let Some(param) =
            super::node_params::node_param(&key.full_name()).filter(|p| !p.writable_over(transport))
        {
            let radio = match transport {
                Transport::Lorawan => "LoRaWAN",
                Transport::P2p => "P2P",
            };
            let instead = if param.shell_only() {
                "the Node's shell"
            } else {
                "NFC"
            };
            return Err(usage(
                transport,
                format!(
                    "{} is not writable over {radio} (sticker-firmware v2.0.0 answers \
                     NOT_WRITABLE); set it over {instead}",
                    key.full_name()
                ),
            ));
        }
    }
    encode_assignment(spec, transport)
}

/// Split `group.field=value` and find the key, aliases included.
fn resolve_assignment(spec: &str, transport: Transport) -> Result<(&'static ConfigKey, &str)> {
    let Some((name, value)) = spec.split_once('=') else {
        return Err(usage(
            transport,
            format!("set-config takes `group.field=value`, got {spec:?}"),
        ));
    };
    let name = name.trim();
    let key = lookup(name).ok_or_else(|| {
        usage(
            transport,
            format!(
                "{name:?} is not a settable config key (node-firmware-v1.4.0.md §7). \
                 Keys are `group.field`, group one of: {}",
                ConfigGroup::ALL
                    .iter()
                    .map(|g| g.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    })?;
    Ok((key, value.trim()))
}

/// The wire half of [`parse_assignment`]: the value checked against its kind and range and
/// encoded as its field, without asking whether the radio may write the key.
fn encode_assignment(spec: &str, transport: Transport) -> Result<Assignment> {
    let (key, value) = resolve_assignment(spec, transport)?;
    let param = super::node_params::node_param(&key.full_name());

    let mut field_bytes = Vec::new();
    match key.kind {
        ValueKind::Secret => {
            // Not "unsupported": the wire supports it. It is refused because the value is key
            // material and this is a command line.
            return Err(usage(
                transport,
                format!(
                    "{} is key material and is never settable from the command line \
                     (system/design.md §10, ledger #22): it would land in argv, in the shell \
                     history and in the process list. Provision keys through node-add's \
                     --keys-file/--keys-stdin",
                    key.full_name()
                ),
            ));
        }
        ValueKind::Bool => {
            let b = match value {
                "true" | "1" => true,
                "false" | "0" => false,
                other => {
                    return Err(usage(
                        transport,
                        format!(
                            "{} takes true/false (or 1/0), got {other:?}",
                            key.full_name()
                        ),
                    ))
                }
            };
            put_bool(&mut field_bytes, key.field, b);
        }
        ValueKind::Uint => {
            let n: u32 = value.parse().map_err(|_| {
                usage(
                    transport,
                    format!(
                        "{} takes a non-negative whole number, got {value:?}",
                        key.full_name()
                    ),
                )
            })?;
            if let Some(p) = param.filter(|p| !p.accepts(i64::from(n))) {
                return Err(usage(
                    transport,
                    format!(
                        "{} takes {}, got {n}",
                        key.full_name(),
                        p.range_text()
                            .unwrap_or_else(|| "a value in range".to_string())
                    ),
                ));
            }
            if is_sensor_type(key) {
                check_sensor_type(n)
                    .map_err(|why| usage(transport, format!("{}: {why}", key.full_name())))?;
            }
            put_uint(&mut field_bytes, key.field, u64::from(n));
        }
        ValueKind::Enum(names) => {
            let lowered = value.to_ascii_lowercase();
            let index = match names.iter().position(|n| *n == lowered) {
                Some(i) => i as u32,
                // A numeric value stays accepted: a firmware that grows a variant this build
                // does not know must still be reachable without a Hub update.
                None => value.parse::<u32>().map_err(|_| {
                    usage(
                        transport,
                        format!(
                            "{} takes one of [{}] (or the numeric wire value), got {value:?}",
                            key.full_name(),
                            names.join(", ")
                        ),
                    )
                })?,
            };
            put_uint(&mut field_bytes, key.field, u64::from(index));
        }
        // An empty value clears one alarm slot: sent as zero-length bytes, which the firmware's
        // nanopb decodes as the all-zero "no rule" (a 1-byte `00` it would refuse).
        ValueKind::Hex { .. } if value.is_empty() && is_alarm_slot(key) => {
            put_bytes(&mut field_bytes, key.field, &[]);
        }
        ValueKind::Hex { bytes } => {
            let mut raw = parse_hex(&value.to_ascii_lowercase())
                .map_err(|why| usage(transport, format!("{}: {why}", key.full_name())))?;
            if is_history_channels(key) {
                check_history_channels(&raw)
                    .map_err(|why| usage(transport, format!("{}: {why}", key.full_name())))?;
                // The firmware's `fill: 0xff`: a short list is the leading columns.
                raw.resize(bytes, HISTORY_CHANNEL_UNUSED);
            }
            if raw.len() != bytes {
                return Err(usage(
                    transport,
                    format!(
                        "{} is {bytes} bytes ({} hex characters), got {} bytes",
                        key.full_name(),
                        bytes * 2,
                        raw.len()
                    ),
                ));
            }
            if is_alarm_slot(key) {
                // Refuse what the Node's `rule_valid` would sanitise away. The slot's current
                // type (a stale rule) is not knowable here; the Portal checks it (§7).
                if let Some(rule) = alarm_rule::Rule::unpack(&raw)
                    .map_err(|why| usage(transport, format!("{}: {why}", key.full_name())))?
                {
                    rule.validate()
                        .map_err(|why| usage(transport, format!("{}: {why}", key.full_name())))?;
                }
            }
            put_bytes(&mut field_bytes, key.field, &raw);
        }
    }

    Ok(Assignment {
        group: key.group,
        name: key.full_name(),
        field_bytes,
    })
}

// ---------------------------------------------------------------------------
// §6 — the command vocabulary
// ---------------------------------------------------------------------------

/// One encodable fPort-85 downlink.
///
/// The NFC/shell-only commands have no variant here at all: the strongest form of "unspellable
/// over radio" is a type that cannot express them. [`Downlink::encode`] still re-runs
/// [`check_radio_policy`], as the belt to this braces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Downlink {
    /// §6 tag 4 — the device reports its `Info`; read it back with `node-get device-info`.
    GetInfo,
    /// §6 tag 5 — one page of the config dump; read it back with `node-get config`.
    /// `page` is omitted from the wire when 0, which is what the bench vector shows.
    GetConfig { page: u32 },
    /// §6 tag 3 — read the named keys back. `fields` are `(group, field)` pairs; the answer is
    /// a `ConfigDump` of just those keys, merged into what the Hub holds (§2.2) — a key the
    /// firmware does not report (an empty alarm slot) simply does not come back.
    GetParam { fields: Vec<(ConfigGroup, u32)> },
    /// Tag 31 (sticker-firmware #429) — the small settings-info the Node sends after boot,
    /// on request: intervals, history, the `cap_*` sensors and the 1-Wire slot types. Read it
    /// back with `node-get config`; its answer merges (`node_reported_state.md` §2.2).
    GetSettings,
    /// §6 tag 2 — write config fields, one or more groups in one frame (each group's encoded
    /// fields, in order). `save` (field 3) persists and reboots after applying.
    /// `replace_alarms` (field 6, sticker-firmware `SetParam.alarms_replace`, nodes_phase1
    /// WP8) clears `alarm_0`–`alarm_15` before this frame's alarm slots apply.
    SetParam {
        groups: Vec<(ConfigGroup, Vec<u8>)>,
        save: bool,
        replace_alarms: bool,
    },
    /// §6 tag 6.
    SettingsSave,
    /// §6 tag 7.
    Reboot,
    /// §6 tag 9 — answered by an extra fPort-2 telemetry uplink, not by an fPort-85 body.
    ForceSend,
    /// §6 tag 10 — an empty selection is refused before encode (§9.7).
    ResetCounters(CounterSelection),
    /// §6 tag 11.
    ReqHistory {
        from_unix: Option<i64>,
        to_unix: Option<i64>,
    },
    /// §6 tag 12 — empty is "re-sync from the network"; a `unix_time` is the one-shot
    /// bootstrap, range-checked.
    ClockSync { unix_time: Option<i64> },
    /// §6 tag 14.
    W1Scan,
    /// §6 tag 16.
    LrwReset,
    /// §6 tag 17.
    LrwJoin,
}

impl Downlink {
    pub fn tag(&self) -> CommandTag {
        match self {
            Downlink::GetInfo => CommandTag::GetInfo,
            Downlink::GetConfig { .. } => CommandTag::GetConfig,
            Downlink::GetSettings => CommandTag::GetSettings,
            Downlink::GetParam { .. } => CommandTag::GetParam,
            Downlink::SetParam { .. } => CommandTag::SetParam,
            Downlink::SettingsSave => CommandTag::SettingsSave,
            Downlink::Reboot => CommandTag::Reboot,
            Downlink::ForceSend => CommandTag::ForceSend,
            Downlink::ResetCounters(_) => CommandTag::ResetCounters,
            Downlink::ReqHistory { .. } => CommandTag::ReqHistory,
            Downlink::ClockSync { .. } => CommandTag::ClockSync,
            Downlink::W1Scan => CommandTag::W1Scan,
            Downlink::LrwReset => CommandTag::LrwReset,
            Downlink::LrwJoin => CommandTag::LrwJoin,
        }
    }

    /// How the §5 result envelope names this downlink.
    pub fn describe(&self) -> String {
        match self {
            Downlink::GetInfo => "get-info".to_string(),
            Downlink::GetConfig { .. } => "get-config".to_string(),
            Downlink::GetSettings => "get-settings".to_string(),
            Downlink::GetParam { .. } => "get-param".to_string(),
            Downlink::SetParam { .. } => "set-config".to_string(),
            other => format!("cmd {}", other.tag().cli_name().unwrap_or("(unnamed)")),
        }
    }

    /// The command's argument sub-message body, without its tag.
    fn body(&self) -> Vec<u8> {
        let mut b = Vec::new();
        match self {
            Downlink::GetInfo
            | Downlink::GetSettings
            | Downlink::SettingsSave
            | Downlink::Reboot
            | Downlink::ForceSend
            | Downlink::W1Scan
            | Downlink::LrwReset
            | Downlink::LrwJoin => {}
            Downlink::GetConfig { page } => {
                // Page 0 is the proto3 default and the bench vector omits it (`2a00`), so a
                // request for the first page is byte-identical to the firmware's own encoder.
                if *page != 0 {
                    put_uint(&mut b, 1, u64::from(*page));
                }
            }
            Downlink::GetParam { fields } => {
                // `GetParam { repeated uint32 lorawan_field = 1; application_field = 2;
                // sensors_field = 3; alarms_field = 4; page = 5; p2p_field = 6; }` — the
                // group numbering `fault_field` uses, except `p2p` (5 is `page`). proto3 packs
                // repeated scalars, as the firmware's own encoder does.
                for group in ConfigGroup::ALL {
                    let mut packed = Vec::new();
                    for (g, f) in fields {
                        if *g == group {
                            put_varint(&mut packed, u64::from(*f));
                        }
                    }
                    if !packed.is_empty() {
                        put_bytes(&mut b, group.get_param_field(), &packed);
                    }
                }
            }
            Downlink::SetParam {
                groups,
                save,
                replace_alarms,
            } => {
                for (group, fields) in groups {
                    put_bytes(&mut b, group.set_param_field(), fields);
                }
                // `save` is field 3 and applies to the whole staged set, so it goes on the
                // last downlink of a multi-frame write only — the caller decides, not this.
                if *save {
                    put_bool(&mut b, 3, true);
                }
                // Field 6: the firmware clears every alarm slot before applying this frame.
                // The first frame of a write only, or it would wipe what an earlier one set.
                if *replace_alarms {
                    put_bool(&mut b, 6, true);
                }
            }
            Downlink::ResetCounters(sel) => {
                // Only the true flags are written: proto3 omits a false bool, and the firmware
                // reads an absent flag as "leave this counter alone".
                if sel.hall_left {
                    put_bool(&mut b, 1, true);
                }
                if sel.hall_right {
                    put_bool(&mut b, 2, true);
                }
                if sel.input_a {
                    put_bool(&mut b, 3, true);
                }
                if sel.input_b {
                    put_bool(&mut b, 4, true);
                }
            }
            Downlink::ReqHistory { from_unix, to_unix } => {
                if let Some(from) = from_unix {
                    put_uint(&mut b, 1, *from as u64);
                }
                if let Some(to) = to_unix {
                    put_uint(&mut b, 2, *to as u64);
                }
            }
            Downlink::ClockSync { unix_time } => {
                if let Some(t) = unix_time {
                    put_uint(&mut b, 1, *t as u64);
                }
            }
        }
        b
    }

    /// Encode `Command { seq, <this> }` — the exact bytes that go on fPort 85.
    ///
    /// **No version byte**: uplinks carry `APP_PROTO_VERSION`, downlinks do not (module note).
    pub fn encode(&self, seq: u32, transport: Transport) -> Result<Vec<u8>> {
        let tag = self.tag();
        // Re-checked even though no variant above can name an NFC-only command: this is the
        // one place every downlink passes through, and a future variant must not be able to
        // skip the guard by being added carefully.
        check_radio_policy(tag, transport)?;
        match self {
            Downlink::ResetCounters(sel) => check_reset_counters(*sel, transport)?,
            Downlink::ClockSync { unix_time } => check_clock_sync_epoch(*unix_time, transport)?,
            Downlink::ReqHistory { from_unix, to_unix } => {
                check_clock_sync_epoch(*from_unix, transport)?;
                check_clock_sync_epoch(*to_unix, transport)?;
                if let (Some(f), Some(t)) = (from_unix, to_unix) {
                    if f > t {
                        return Err(usage(
                            transport,
                            format!("history-replay --from {f} is after --to {t}"),
                        ));
                    }
                }
            }
            _ => {}
        }

        let mut out = Vec::new();
        put_uint(&mut out, 1, u64::from(seq));
        put_bytes(&mut out, tag as u32, &self.body());
        Ok(out)
    }
}

/// What one `node-send <type>` invocation puts on the air.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The frames, in send order.
    pub downlinks: Vec<Downlink>,
    /// `set-config`'s canonical `group.field` names, per frame, positionally matching
    /// `downlinks`. Empty for every other form.
    pub keys: Vec<Vec<String>>,
    /// How the §5 result envelope names what was sent.
    pub label: String,
    /// A sequence number the operator asked for (`get-config --seq`), used instead of a fresh
    /// one. Only a single-frame plan carries it.
    pub seq: Option<u32>,
}

/// Turn the parsed `node-send <type>` subcommand into a [`Plan`].
///
/// A `set-config` packs its keys into as few `SetParam` frames as the downlink budget allows
/// (`plan` assumes the carrier's smallest: DR0's 51 B on LoRaWAN, [`P2P_BUDGET`] on P2P;
/// [`plan_with_budget`] takes the Node's own). `save` rides the
/// last frame, so the whole staged set is committed once (the firmware's own documented
/// batching rule) rather than the operator having to remember `cmd settings-save`.
///
/// One function rather than a plan-then-name pair: parsing an assignment twice would mean two
/// chances for the two answers to disagree about what the operator asked for.
pub fn plan(kind: &NodeSendKind, transport: Transport) -> Result<Plan> {
    plan_with_budget(kind, transport, carrier_downlink_budget(transport, None))
}

/// [`plan`] for a Node whose downlink budget is known (see [`downlink_budget`]); only
/// `set-config` depends on it.
pub fn plan_with_budget(kind: &NodeSendKind, transport: Transport, budget: usize) -> Result<Plan> {
    let one = |d: Downlink| Plan {
        label: d.describe(),
        downlinks: vec![d],
        keys: Vec::new(),
        seq: None,
    };
    match kind {
        NodeSendKind::GetInfo => Ok(one(Downlink::GetInfo)),
        NodeSendKind::GetConfig { page, seq } => Ok(Plan {
            seq: *seq,
            ..one(Downlink::GetConfig { page: *page })
        }),
        NodeSendKind::GetSettings => Ok(one(Downlink::GetSettings)),
        NodeSendKind::GetParam { params } => {
            let mut fields = Vec::new();
            for name in params {
                let key = lookup(name).ok_or_else(|| {
                    usage(
                        transport,
                        format!(
                            "{name:?} is not a config key (node-firmware-v1.4.0.md §7). Keys are \
                             `group.field`, group one of: {}",
                            ConfigGroup::ALL
                                .iter()
                                .map(|g| g.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )
                })?;
                if key.kind == ValueKind::Secret {
                    // The firmware does not send key material over the radio (§2.2 Q1); asking
                    // for it would only spend an RX window on an answer that cannot carry it.
                    return Err(usage(
                        transport,
                        format!(
                            "{} is key material and is never read over the radio",
                            key.full_name()
                        ),
                    ));
                }
                if !fields.contains(&(key.group, key.field)) {
                    fields.push((key.group, key.field));
                }
            }
            if fields.is_empty() {
                return Err(usage(
                    transport,
                    "get-param needs at least one `group.field`",
                ));
            }
            for group in ConfigGroup::ALL {
                let n = fields.iter().filter(|(g, _)| *g == group).count();
                if n > GET_PARAM_MAX_PER_GROUP {
                    return Err(usage(
                        transport,
                        format!(
                            "get-param asks for {n} {} keys; the Node takes at most \
                             {GET_PARAM_MAX_PER_GROUP} per group in one frame",
                            group.as_str()
                        ),
                    ));
                }
            }
            Ok(one(Downlink::GetParam { fields }))
        }
        NodeSendKind::Cmd(args) => Ok(one(from_cmd(args, transport)?)),
        NodeSendKind::SetConfig {
            assignments,
            replace_alarms,
            ..
        } => plan_set_config(assignments, *replace_alarms, budget, transport),
        // `0x91` control commands, not fPort 85: the P2P arm queues them itself
        // (p2p_control_commands.md §2). LoRaWAN has no such command: its MAC is ChirpStack's.
        NodeSendKind::DevStatus | NodeSendKind::Backoff { .. } | NodeSendKind::Rejoin { .. } => {
            Err(usage(
                transport,
                format!(
                    "node-send {} is a P2P control command (0x91); on LoRaWAN the network \
                     server owns the MAC layer, so there is nothing to send",
                    kind.control().map_or("?", |c| c.name())
                ),
            ))
        }
    }
}

/// A frame the budget is measured on carries the widest `seq` varint a Hub will allocate for
/// years (21 bits), so a later, longer `seq` cannot push a planned frame over.
const SIZING_SEQ: u32 = (1 << 21) - 1;

/// `max_count` of every `Command.GetParam.<group>_field` in the firmware's nanopb options
/// (`app_config.options.in`). One key more and the Node cannot decode the frame at all.
const GET_PARAM_MAX_PER_GROUP: usize = 16;

/// The FRMPayload a downlink may carry at the Node's data rate (EU868, LoRaWAN Regional
/// Parameters): DR0–2 51 B, DR3 115 B, DR4–7 222 B. An unknown rate plans for DR0. ChirpStack
/// adds MAC commands only where they fit, so the application payload may use the whole size.
pub fn downlink_budget(dr: Option<u64>) -> usize {
    match dr {
        Some(3) => 115,
        Some(4..=7) => 222,
        _ => DR0_BUDGET,
    }
}

/// The smallest downlink budget, and the one `plan` assumes when the caller does not know the
/// Node's data rate.
pub const DR0_BUDGET: usize = 51;

/// The command body one P2P downlink carries: STICKER FW v2.0.0 `P2P_MAX_BODY`
/// (`app_radio_p2p.h`), 72 B on every PHY profile (96 B frame, 74 B TOWER payload; #486 U-1).
/// P2P has no data-rate limit, only duty-cycle airtime, so the budget is fixed. The Hub side
/// carries the same 72 B ([`crate::p2p::tower::MAX_COMMAND_BODY`]).
pub const P2P_BUDGET: usize = crate::p2p::tower::MAX_COMMAND_BODY;

/// The budget `node-send` packs a `set-config` into on `transport`: on LoRaWAN the Node's last
/// data rate's ([`downlink_budget`]), on P2P always [`P2P_BUDGET`] whatever the data rate
/// (amended 2026-10-15: P2P packed at DR0's 51 B before). The Portal's draft plans with this too, so the frame count it
/// shows is the one the Hub sends on either carrier (PU-4).
pub fn carrier_downlink_budget(transport: Transport, dr: Option<u64>) -> usize {
    match transport {
        Transport::Lorawan => downlink_budget(dr),
        Transport::P2p => P2P_BUDGET,
    }
}

/// Refuse a plan with a frame the Node cannot receive at `budget` (#129).
///
/// ChirpStack accepts any size into the queue and only at transmit time deletes an item over
/// the data rate's maximum, so the operator was told `queued` for a frame that never left.
/// Only `set-config` packs by the budget; a `get-param` naming many groups, or a long `cmd`,
/// could still overflow it. Measured as the budget is planned on ([`SIZING_SEQ`]).
pub fn check_budget(
    plan: &Plan,
    transport: Transport,
    budget: usize,
    dr: Option<u64>,
) -> Result<()> {
    for d in &plan.downlinks {
        let size = d.encode(SIZING_SEQ, transport)?.len();
        if size > budget {
            return Err(too_big(transport, &d.describe(), size, budget, dr));
        }
    }
    Ok(())
}

/// [`check_budget`] for a raw `--hex` payload: the limit is the FRMPayload's on any port.
pub fn check_raw_budget(
    len: usize,
    transport: Transport,
    budget: usize,
    dr: Option<u64>,
) -> Result<()> {
    if len > budget {
        return Err(too_big(transport, "the --hex payload", len, budget, dr));
    }
    Ok(())
}

fn too_big(transport: Transport, what: &str, size: usize, budget: usize, dr: Option<u64>) -> Error {
    let rate = match (transport, dr) {
        (Transport::P2p, _) => "P2P (a fixed budget, any data rate)".to_string(),
        (Transport::Lorawan, Some(dr)) => format!("DR{dr}, the Node's last data rate"),
        (Transport::Lorawan, None) => {
            "the Node's data rate (unknown, so DR0 is assumed)".to_string()
        }
    };
    usage(
        transport,
        format!(
            "this command cannot be sent: {what} is {size} B, but a downlink at {rate} \
             carries at most {budget} B and the network server would discard it unsent. \
             Ask for fewer keys or split the command"
        ),
    )
}

/// `set-config`: as few `SetParam` frames as the budget allows, in the operator's order. Keys
/// of one group share its sub-message; a frame closes when the next key would not fit. `save`
/// rides the last frame (the firmware commits the whole staged set once), `replace_alarms`
/// the first (it clears every slot, so a later frame must not repeat it). A replace with no
/// key is one frame that only clears.
fn plan_set_config(
    assignments: &[String],
    replace_alarms: bool,
    budget: usize,
    transport: Transport,
) -> Result<Plan> {
    let parsed = assignments
        .iter()
        .map(|a| parse_assignment(a, transport))
        .collect::<Result<Vec<_>>>()?;
    if parsed.is_empty() && !replace_alarms {
        // clap already makes the list `required = true` without --replace-alarms; this is the
        // direct library caller's refusal, and it is the same InvalidArgument either way.
        return Err(usage(
            transport,
            "set-config needs at least one `group.field=value`",
        ));
    }

    struct Frame {
        groups: Vec<(ConfigGroup, Vec<u8>)>,
        keys: Vec<String>,
    }
    impl Frame {
        fn with(&self, a: &Assignment) -> Vec<(ConfigGroup, Vec<u8>)> {
            let mut groups = self.groups.clone();
            match groups.iter_mut().find(|(g, _)| *g == a.group) {
                Some((_, bytes)) => bytes.extend_from_slice(&a.field_bytes),
                None => groups.push((a.group, a.field_bytes.clone())),
            }
            groups
        }
    }
    let size = |groups: Vec<(ConfigGroup, Vec<u8>)>, first: bool| -> Result<usize> {
        Ok(Downlink::SetParam {
            groups,
            save: true,
            replace_alarms: replace_alarms && first,
        }
        .encode(SIZING_SEQ, transport)?
        .len())
    };

    let empty = || Frame {
        groups: Vec::new(),
        keys: Vec::new(),
    };
    let mut frames: Vec<Frame> = Vec::new();
    let mut current = empty();
    for a in parsed {
        let first = frames.is_empty();
        if size(current.with(&a), first)? <= budget {
            current.groups = current.with(&a);
            current.keys.push(a.name);
            continue;
        }
        // It does not fit beside what the frame holds: it opens the next frame, which must
        // be able to hold it alone.
        let first = first && current.keys.is_empty();
        if size(empty().with(&a), first)? > budget {
            return Err(usage(
                transport,
                format!(
                    "{} does not fit one {budget}-byte downlink at the Node's data rate",
                    a.name
                ),
            ));
        }
        if !current.keys.is_empty() {
            frames.push(std::mem::replace(&mut current, empty()));
        }
        current.groups = current.with(&a);
        current.keys.push(a.name);
    }
    frames.push(current);

    let last = frames.len() - 1;
    let keys = frames.iter().map(|f| f.keys.clone()).collect();
    let downlinks = frames
        .into_iter()
        .enumerate()
        .map(|(i, f)| Downlink::SetParam {
            groups: f.groups,
            save: i == last,
            replace_alarms: replace_alarms && i == 0,
        })
        .collect();
    Ok(Plan {
        downlinks,
        keys,
        label: "set-config".to_string(),
        seq: None,
    })
}

/// The size of one on-device alarm rule (`alarms.alarm_N`).
const ALARM_SLOT_BYTES: usize = alarm_rule::RULE_LEN;

/// `application.history_channels` (435273c6): up to 24 columns `slot << 5 | ch`.
fn is_history_channels(key: &ConfigKey) -> bool {
    key.group == ConfigGroup::Application && key.name == "history_channels"
}

/// `sensors.sensor1..4_type` (23..26): the type configured on a 1-Wire slot.
fn is_sensor_type(key: &ConfigKey) -> bool {
    key.group == ConfigGroup::Sensors
        && key
            .name
            .strip_prefix("sensor")
            .and_then(|r| r.strip_suffix("_type"))
            .is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit())
}

/// A slot type is 0 (no sensor) or a 1-Wire type of the registry; the motherboard id or an id
/// this build does not know would configure a slot the Node cannot read.
fn check_sensor_type(n: u32) -> std::result::Result<(), String> {
    let ok = n == 0
        || u8::try_from(n)
            .ok()
            .and_then(super::sensor_types::sensor_type)
            .is_some_and(|t| t.is_w1());
    if ok {
        return Ok(());
    }
    let known: Vec<String> = super::sensor_types::TYPES
        .iter()
        .filter(|t| t.is_w1())
        .map(|t| format!("{} {}", t.id, t.name))
        .collect();
    Err(format!(
        "takes 0 (no sensor) or a 1-Wire type id [{}], got {n}",
        known.join(", ")
    ))
}

/// An unused `history_channels` column.
pub const HISTORY_CHANNEL_UNUSED: u8 = 0xff;

/// Every used column names slot 0..4 (`slot << 5 | ch`, so `ch` is 0..31 by construction),
/// at most once, and the list is not longer than the field. A slot-0 column must be a
/// motherboard channel of the registry; a 1-Wire column is checked by slot only, its type
/// being the slot's configuration at sample time.
fn check_history_channels(raw: &[u8]) -> std::result::Result<(), String> {
    if raw.len() > 24 {
        return Err(format!("at most 24 columns, got {}", raw.len()));
    }
    let mut seen = std::collections::BTreeSet::new();
    for &b in raw.iter().filter(|b| **b != HISTORY_CHANNEL_UNUSED) {
        if b >> 5 > super::sensor_types::W1_SLOTS {
            return Err(format!("column {b:#04x} names slot {}, not 0..4", b >> 5));
        }
        if b >> 5 == 0 && super::sensor_types::channel(0, None, b & 0x1f).is_none() {
            return Err(format!(
                "column {b:#04x} names motherboard channel {}, which does not exist",
                b & 0x1f
            ));
        }
        if !seen.insert(b) {
            return Err(format!("column {b:#04x} is listed twice"));
        }
    }
    Ok(())
}

fn is_alarm_slot(key: &ConfigKey) -> bool {
    key.group == ConfigGroup::Alarms
        && matches!(key.kind, ValueKind::Hex { bytes } if bytes == ALARM_SLOT_BYTES)
}

/// `node-send cmd <name>` → one downlink.
fn from_cmd(args: &NodeSendCmdArgs, transport: Transport) -> Result<Downlink> {
    let name = args.name.as_str();
    let tag = CommandTag::from_cli_name(name).ok_or_else(|| {
        let spellable: Vec<&str> = crate::decode::command::ALL
            .iter()
            .filter_map(|t| t.cli_name())
            .collect();
        usage(
            transport,
            format!(
                "{name:?} is not a command this radio can send. Available: [{}]. The NFC/shell-\
                 only operations (factory_reset, set_secret_key, req_history_page, \
                 device_reset) deliberately have no spelling at all — they are refused over \
                 radio entirely (node-firmware-v1.4.0.md §6)",
                spellable.join(", ")
            ),
        )
    })?;
    // Belt: `from_cli_name` only ever answers with a spellable tag, and the spellable set is
    // exactly the radio-allowed one. Braces: the policy table is the authority, so ask it.
    check_radio_policy(tag, transport)?;

    // Arguments that belong to another command are refused rather than ignored — silently
    // dropping `--from` on a `reboot` would tell the operator a window was honoured.
    let reject_extras = |allowed: &str| -> Result<()> {
        if args.from.is_some() || args.to.is_some() {
            return Err(usage(
                transport,
                format!("--from/--to belong to `cmd history-replay`, not `cmd {allowed}`"),
            ));
        }
        if !args.counter.is_empty() {
            return Err(usage(
                transport,
                format!("--counter belongs to `cmd reset-counters`, not `cmd {allowed}`"),
            ));
        }
        Ok(())
    };

    match tag {
        CommandTag::ReqHistory => {
            if !args.counter.is_empty() {
                return Err(usage(
                    transport,
                    "--counter belongs to `cmd reset-counters`, not `cmd history-replay`",
                ));
            }
            Ok(Downlink::ReqHistory {
                from_unix: args.from,
                to_unix: args.to,
            })
        }
        CommandTag::ResetCounters => {
            if args.from.is_some() || args.to.is_some() {
                return Err(usage(
                    transport,
                    "--from/--to belong to `cmd history-replay`, not `cmd reset-counters`",
                ));
            }
            let mut sel = CounterSelection::default();
            for c in &args.counter {
                match c {
                    CounterName::HallLeft => sel.hall_left = true,
                    CounterName::HallRight => sel.hall_right = true,
                    CounterName::InputA => sel.input_a = true,
                    CounterName::InputB => sel.input_b = true,
                }
            }
            // §9.7's mandatory guard, run here so the refusal names the missing flag rather
            // than arriving as a bare encode failure.
            check_reset_counters(sel, transport)?;
            Ok(Downlink::ResetCounters(sel))
        }
        CommandTag::ClockSync => {
            reject_extras("clock-sync")?;
            // The empty form: "re-sync from the network". The one-shot `{unix_time}` bootstrap
            // is an NFC flow (the phone supplies the wall clock) and has no CLI spelling —
            // `Downlink::ClockSync` still carries it, range-checked, for a library caller.
            Ok(Downlink::ClockSync { unix_time: None })
        }
        CommandTag::Reboot => reject_extras("reboot").map(|()| Downlink::Reboot),
        CommandTag::SettingsSave => reject_extras("settings-save").map(|()| Downlink::SettingsSave),
        CommandTag::ForceSend => reject_extras("force-send").map(|()| Downlink::ForceSend),
        CommandTag::W1Scan => reject_extras("w1-scan").map(|()| Downlink::W1Scan),
        CommandTag::LrwReset => reject_extras("lrw-reset").map(|()| Downlink::LrwReset),
        CommandTag::LrwJoin => reject_extras("lrw-join").map(|()| Downlink::LrwJoin),
        other => Err(Error::new(
            Kind::Phase2NotImplemented,
            transport,
            Component::ControlSocket,
            format!("{other:?} has a CLI spelling but no encoder arm yet"),
        )),
    }
}

/// Allocate the next sequence for a device, from [`SeqAllocator`].
///
/// `previous` is what the device's cache document last recorded; `None` on the first command
/// this Hub ever sent it. The allocator is the one in [`crate::decode::command`] — the
/// correlation rule lives in one place, and a second counter here would be a second rule.
pub fn next_seq(previous: Option<u32>) -> u32 {
    let mut allocator = SeqAllocator::from_last(previous.unwrap_or(0));
    allocator.allocate()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #486 U-1 audit: every fixed-shape downlink, at its widest (`seq` u32::MAX, the latest
    /// epoch, every flag), fits one P2P frame's 72 B body; `set-config` frames are packed to
    /// [`P2P_BUDGET`] (that same 72 B) on P2P, so they fit too.
    #[test]
    fn every_fixed_downlink_fits_one_p2p_frame() {
        use crate::decode::command::{CounterSelection, CLOCK_SYNC_MAX_UNIX};
        let t = Some(CLOCK_SYNC_MAX_UNIX);
        let all = [
            Downlink::GetInfo,
            Downlink::GetConfig { page: u32::MAX },
            Downlink::GetSettings,
            Downlink::SettingsSave,
            Downlink::Reboot,
            Downlink::ForceSend,
            Downlink::ResetCounters(CounterSelection {
                hall_left: true,
                hall_right: true,
                input_a: true,
                input_b: true,
            }),
            Downlink::ReqHistory {
                from_unix: t,
                to_unix: t,
            },
            Downlink::ClockSync { unix_time: t },
            Downlink::W1Scan,
            Downlink::LrwReset,
            Downlink::LrwJoin,
        ];
        let budget = crate::p2p::tower::MAX_COMMAND_BODY;
        assert_eq!(budget, 72);
        for d in all {
            match d.encode(u32::MAX, Transport::P2p) {
                Ok(b) => assert!(b.len() <= 20, "{d:?}: {} B", b.len()),
                // A command P2P refuses by policy never reaches the frame.
                Err(e) => assert_eq!(e.kind(), crate::error::Kind::InvalidArgument, "{d:?}"),
            }
        }
        assert_eq!(carrier_downlink_budget(Transport::P2p, None), budget);
        assert_eq!(carrier_downlink_budget(Transport::P2p, Some(0)), budget);
        let plan = plan(
            &NodeSendKind::SetConfig {
                assignments: vec![format!(
                    "alarms.alarm_0=00{}",
                    "ab".repeat(ALARM_SLOT_BYTES - 1)
                )],
                replace_alarms: true,
                supersede: false,
            },
            Transport::P2p,
        )
        .expect("plan");
        for d in &plan.downlinks {
            assert!(d.encode(u32::MAX, Transport::P2p).unwrap().len() <= budget);
        }
    }

    /// The Node decodes at most 16 keys of one group per `GetParam`; the 17th is refused
    /// here rather than answered with the firmware's decode error (#486 HIL, 2026-10-12).
    #[test]
    fn get_param_refuses_more_than_sixteen_keys_of_one_group() {
        let sensors: Vec<String> = CONFIG_KEYS
            .iter()
            .filter(|k| k.group == ConfigGroup::Sensors && k.kind != ValueKind::Secret)
            .map(|k| k.full_name())
            .collect();
        assert!(sensors.len() > GET_PARAM_MAX_PER_GROUP);
        let ask = |n: usize| {
            plan(
                &NodeSendKind::GetParam {
                    params: sensors[..n].to_vec(),
                },
                Transport::P2p,
            )
        };
        assert!(ask(GET_PARAM_MAX_PER_GROUP).is_ok());
        let err = ask(GET_PARAM_MAX_PER_GROUP + 1).expect_err("17 sensors keys");
        assert_eq!(err.kind(), crate::error::Kind::InvalidArgument);
    }

    /// #129: a `get-param` within the per-group cap but over the DR0 budget (HIL #494: 40 keys
    /// in five groups) is refused before ChirpStack can discard it unsent; at DR5 it goes.
    #[test]
    fn a_frame_over_the_downlink_budget_is_refused_before_it_is_queued() {
        let mut params = Vec::new();
        for group in ConfigGroup::ALL {
            params.extend(
                CONFIG_KEYS
                    .iter()
                    .filter(|k| k.group == group && k.kind != ValueKind::Secret)
                    .take(GET_PARAM_MAX_PER_GROUP)
                    .map(|k| k.full_name()),
            );
        }
        let planned = plan(&NodeSendKind::GetParam { params }, Transport::Lorawan).expect("plans");
        let size = planned.downlinks[0]
            .encode(SIZING_SEQ, Transport::Lorawan)
            .expect("encodes")
            .len();
        assert!(
            size > DR0_BUDGET && size <= downlink_budget(Some(5)),
            "{size} B"
        );

        let err = check_budget(
            &planned,
            Transport::Lorawan,
            downlink_budget(Some(0)),
            Some(0),
        )
        .expect_err("over DR0");
        assert_eq!(err.kind(), crate::error::Kind::InvalidArgument);
        assert!(err.to_string().contains("cannot be sent"), "{err}");
        assert!(check_budget(
            &planned,
            Transport::Lorawan,
            downlink_budget(Some(5)),
            Some(5)
        )
        .is_ok());

        assert!(check_raw_budget(51, Transport::Lorawan, DR0_BUDGET, None).is_ok());
        assert!(check_raw_budget(52, Transport::Lorawan, DR0_BUDGET, None).is_err());
    }

    /// #96: a missing page of a paged config is asked for with `get-config --page N`.
    #[test]
    fn get_config_can_start_at_a_later_page() {
        let plan = plan(
            &NodeSendKind::GetConfig {
                page: 1,
                seq: Some(9),
            },
            Transport::Lorawan,
        )
        .expect("plan");
        assert_eq!(plan.seq, Some(9));
        assert_eq!(plan.downlinks.len(), 1);
        assert_eq!(encode(&plan.downlinks[0], 9), "08092a020801");
        let first = plan_first(&NodeSendKind::GetConfig { page: 0, seq: None });
        assert_eq!(encode(&first, 8), "08082a00", "page 0 stays byte-identical");
    }

    fn plan_first(kind: &NodeSendKind) -> Downlink {
        plan(kind, Transport::Lorawan)
            .expect("plan")
            .downlinks
            .remove(0)
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn encode(d: &Downlink, seq: u32) -> String {
        hex(&d.encode(seq, Transport::Lorawan).expect("encodes"))
    }

    /// The bench table, byte for byte — every row `tests/integration/README.md` records from
    /// the firmware's own `ttn.js encodeDownlink`, sent to a real Node on 2026-08-25.
    #[test]
    fn the_bench_vectors_encode_byte_identically() {
        assert_eq!(encode(&Downlink::GetInfo, 7), "08072200");
        assert_eq!(encode(&Downlink::GetConfig { page: 0 }, 8), "08082a00");
        assert_eq!(encode(&Downlink::GetConfig { page: 1 }, 9), "08092a020801");
        assert_eq!(encode(&Downlink::GetSettings, 7), "0807fa0100");
        // The bench frame the Hub controller sent by hand on 2026-09-23 (seq 116), which the
        // Node answered with `alarms.alarm_limit`: alarms fields [1 alarm_limit, 3 alarm_0].
        assert_eq!(
            encode(
                &Downlink::GetParam {
                    fields: vec![(ConfigGroup::Alarms, 1), (ConfigGroup::Alarms, 3)]
                },
                116
            ),
            "08741a0422020103"
        );
        assert_eq!(
            encode(&Downlink::ClockSync { unix_time: None }, 11),
            "080b6200"
        );
        assert_eq!(encode(&Downlink::W1Scan, 12), "080c7200");
        assert_eq!(encode(&Downlink::ForceSend, 14), "080e4a00");
    }

    /// Not in the bench table, but the same shape and the same authority (§6's tag column):
    /// an empty argument message is `tag, 0x00`, and a tag above 15 takes two bytes.
    #[test]
    fn the_remaining_empty_commands_follow_the_same_shape() {
        assert_eq!(encode(&Downlink::Reboot, 1), "08013a00");
        assert_eq!(encode(&Downlink::SettingsSave, 2), "08023200");
        // 16 << 3 | 2 = 130, which is a two-byte varint tag — the case a single-byte cast
        // would have silently mangled.
        assert_eq!(encode(&Downlink::LrwReset, 3), "0803820100");
        assert_eq!(encode(&Downlink::LrwJoin, 4), "08048a0100");
    }

    #[test]
    fn a_seq_above_127_is_a_multi_byte_varint() {
        assert_eq!(encode(&Downlink::GetInfo, 300), "08ac022200");
    }

    /// §9.7: `device_reset` (8), `factory_reset` (23), `set_secret_key` (24) and
    /// `req_history_page` (13) are refused over radio entirely. The bench proved the firmware
    /// agrees — `080d4200` came back `error { code: 3, "transport not allowed" }`.
    #[test]
    fn the_nfc_only_commands_have_no_encoder_and_no_spelling() {
        for name in [
            "device-reset",
            "factory-reset",
            "set-secret-key",
            "req-history-page",
            "device_reset",
        ] {
            let args = NodeSendCmdArgs {
                name: name.to_string(),
                from: None,
                to: None,
                counter: Vec::new(),
            };
            let err = from_cmd(&args, Transport::Lorawan).expect_err("unspellable");
            assert_eq!(err.kind(), Kind::InvalidArgument, "{name}");
            assert!(
                err.message().contains("refused over radio")
                    || err.message().contains("no spelling at all"),
                "{name}: {}",
                err.message()
            );
        }
        // And the tags themselves cannot be reached through the policy check either.
        for tag in [
            CommandTag::DeviceReset,
            CommandTag::FactoryReset,
            CommandTag::SetSecretKey,
            CommandTag::ReqHistoryPage,
        ] {
            assert!(
                check_radio_policy(tag, Transport::Lorawan).is_err(),
                "{tag:?}"
            );
            assert_eq!(tag.cli_name(), None, "{tag:?}");
        }
    }

    /// `set-config` writes into the group's own `SetParam` field, which is **not** the field
    /// number the same group has in a `ConfigDump`.
    #[test]
    fn set_config_encodes_into_the_set_param_group_field() {
        let planned = plan(
            &NodeSendKind::SetConfig {
                assignments: vec!["application.interval_report=900".to_string()],
                replace_alarms: false,
                supersede: false,
            },
            Transport::Lorawan,
        )
        .expect("plans");
        assert_eq!(planned.downlinks.len(), 1);
        assert_eq!(planned.label, "set-config");
        assert_eq!(
            planned.keys,
            vec![vec!["application.interval_report".to_string()]]
        );
        // 08 05                seq 5
        // 12 07                set_param, 7 bytes
        //    12 03 18 84 07      application (field 2), 3 bytes: interval_report (3) = 900
        //    18 01               save = true (last frame of the write)
        assert_eq!(encode(&planned.downlinks[0], 5), "0805120712031884071801");
    }

    fn set_config(assignments: &[&str], replace_alarms: bool) -> NodeSendKind {
        NodeSendKind::SetConfig {
            assignments: assignments.iter().map(|a| a.to_string()).collect(),
            replace_alarms,
            supersede: false,
        }
    }

    fn flags(planned: &Plan) -> Vec<(bool, bool)> {
        planned
            .downlinks
            .iter()
            .map(|d| match d {
                Downlink::SetParam {
                    save,
                    replace_alarms,
                    ..
                } => (*save, *replace_alarms),
                other => panic!("not a SetParam: {other:?}"),
            })
            .collect()
    }

    /// A write that fits one frame is one frame: the groups share it, each in its own
    /// `SetParam` field, and `save` closes it.
    #[test]
    fn a_multi_key_write_that_fits_is_one_frame() {
        let planned = plan(
            &set_config(
                &[
                    "application.interval_sample=60",
                    "sensors.cap_barometer=true",
                    "alarms.alarm_limit=30",
                ],
                false,
            ),
            Transport::Lorawan,
        )
        .expect("plans");
        assert_eq!(
            planned.downlinks.len(),
            1,
            "three small keys fit DR0's 51 B"
        );
        assert_eq!(
            planned.keys,
            vec![vec![
                "application.interval_sample".to_string(),
                "sensors.cap_barometer".to_string(),
                "alarms.alarm_limit".to_string(),
            ]]
        );
        // 08 02 seq 2 · 12 0e set_param · 12 02 10 3c application: interval_sample = 60 ·
        // 22 02 30 01 sensors: cap_barometer = true · 2a 02 08 1e alarms: alarm_limit = 30 ·
        // 18 01 save.
        assert_eq!(
            encode(&planned.downlinks[0], 2),
            "0802120e1202103c220230012a02081e1801"
        );
    }

    /// Over budget, the keys spill into further frames in order; `save` rides the last only,
    /// and every frame fits.
    #[test]
    fn a_write_over_budget_splits_and_saves_once() {
        let kind = set_config(
            &[
                "application.interval_sample=60",
                "sensors.cap_barometer=true",
                "alarms.alarm_limit=30",
            ],
            false,
        );
        let planned = plan_with_budget(&kind, Transport::Lorawan, 14).expect("plans");
        assert!(
            planned.downlinks.len() > 1,
            "a 14-byte budget cannot hold all three"
        );
        assert_eq!(
            planned.keys.iter().flatten().count(),
            3,
            "every key is sent, once"
        );
        let saves: Vec<bool> = flags(&planned).iter().map(|(save, _)| *save).collect();
        assert_eq!(saves.iter().filter(|s| **s).count(), 1);
        assert_eq!(saves.last(), Some(&true), "the last frame commits");
        for d in &planned.downlinks {
            assert!(d.encode(SIZING_SEQ, Transport::Lorawan).unwrap().len() <= 14);
        }
    }

    fn slot(i: usize) -> String {
        format!("alarms.alarm_{i}=0300000100000000a0410000c84100000000")
    }

    /// The alarm table (nodes_phase1.cs.md WP2): `alarms_replace` (SetParam field 6) clears
    /// every slot, so the portal sends only the filled ones, and on its own it clears all.
    #[test]
    fn a_replace_alone_is_one_frame_that_clears() {
        let planned = plan(&set_config(&[], true), Transport::Lorawan).expect("plans");
        assert_eq!(flags(&planned), [(true, true)]);
        // 08 05 seq 5 · 12 04 set_param · 18 01 save · 30 01 alarms_replace (field 6)
        assert_eq!(encode(&planned.downlinks[0], 5), "0805120418013001");
    }

    /// P2P packs at the fixed 72 B body (FW v2.0.0 `P2P_MAX_BODY`), not at DR0's 51 B:
    /// four two-sided critical rules with `alarms_replace` and `save` are two frames.
    #[test]
    fn four_critical_rules_on_p2p_are_two_frames() {
        let four: Vec<String> = (0..4).map(slot).collect();
        let four: Vec<&str> = four.iter().map(String::as_str).collect();
        let planned = plan(&set_config(&four, true), Transport::P2p).expect("plans");
        assert_eq!(flags(&planned), [(false, true), (true, false)]);
        let sizes: Vec<usize> = planned
            .downlinks
            .iter()
            .map(|d| d.encode(SIZING_SEQ, Transport::P2p).unwrap().len())
            .collect();
        eprintln!("p2p frame sizes: {sizes:?}");
        assert!(sizes.iter().all(|&n| n <= P2P_BUDGET), "{sizes:?}");
        assert!(sizes.iter().any(|&n| n > DR0_BUDGET), "{sizes:?}");
        // The same table at LoRaWAN DR0 takes more frames.
        let dr0 = plan(&set_config(&four, true), Transport::Lorawan).expect("plans");
        assert!(dr0.downlinks.len() > planned.downlinks.len());
    }

    #[test]
    fn a_replace_rides_the_first_frame_and_save_the_last() {
        // DR0: the 18-byte rule of v2.0.0 is 20 B on the wire, so one filled slot rides with
        // the replace (two did with the 17-byte v1.4 rule) and the next frame holds two.
        let three: Vec<String> = (0..3).map(slot).collect();
        let three: Vec<&str> = three.iter().map(String::as_str).collect();
        let planned = plan(&set_config(&three, true), Transport::Lorawan).expect("plans");
        assert_eq!(flags(&planned), [(false, true), (true, false)]);
        assert_eq!(planned.keys[0].len(), 1);
        assert_eq!(planned.keys[1].len(), 2);
        for d in &planned.downlinks {
            assert!(d.encode(SIZING_SEQ, Transport::Lorawan).unwrap().len() <= DR0_BUDGET);
        }

        // DR4-5: ten filled slots are one frame, the eleventh spills.
        let ten: Vec<String> = (0..10).map(slot).collect();
        let ten: Vec<&str> = ten.iter().map(String::as_str).collect();
        let at = |dr| {
            plan_with_budget(
                &set_config(&ten, true),
                Transport::Lorawan,
                downlink_budget(Some(dr)),
            )
            .expect("plans")
        };
        assert_eq!(flags(&at(5)), [(true, true)]);
        assert_eq!(at(3).downlinks.len(), 2, "DR3's 115 B holds five");
    }

    #[test]
    fn the_downlink_budget_follows_the_data_rate() {
        assert_eq!(downlink_budget(None), 51, "unknown plans for DR0");
        assert_eq!(downlink_budget(Some(0)), 51);
        assert_eq!(downlink_budget(Some(2)), 51);
        assert_eq!(downlink_budget(Some(3)), 115);
        assert_eq!(downlink_budget(Some(5)), 222);
        assert_eq!(downlink_budget(Some(7)), 222);
    }

    #[test]
    /// FW v2.0.0 `P2P_MAX_BODY`: a fixed 72 B body on P2P, no data-rate limit.
    fn a_p2p_set_config_packs_at_72_b_whatever_the_rate() {
        for dr in [None, Some(0), Some(3), Some(5)] {
            assert_eq!(
                carrier_downlink_budget(Transport::Lorawan, dr),
                downlink_budget(dr)
            );
            assert_eq!(carrier_downlink_budget(Transport::P2p, dr), P2P_BUDGET);
        }
    }

    /// An empty value clears one slot as zero-length bytes, which the firmware's nanopb reads
    /// as all zeros. A 1-byte `00` it would refuse, so that stays an error here too.
    #[test]
    fn an_empty_alarm_slot_is_zero_length_bytes() {
        let planned =
            plan(&set_config(&["alarms.alarm_3="], false), Transport::Lorawan).expect("plans");
        // 08 05 · 12 06 set_param · 2a 02 alarms (field 5) · 32 00 alarm_3 (field 6), 0 B ·
        // 18 01 save
        assert_eq!(encode(&planned.downlinks[0], 5), "080512062a0232001801");
        assert!(plan(
            &set_config(&["alarms.alarm_3=00"], false),
            Transport::Lorawan
        )
        .is_err());
        assert!(
            plan(
                &set_config(&["sensors.sensor1_rom="], false),
                Transport::Lorawan
            )
            .is_err(),
            "only an alarm slot has an empty form"
        );
    }

    #[test]
    fn every_value_kind_round_trips_through_the_wire_encoding() {
        let cases = [
            // bool false is still written explicitly: the operator asked for false, and an
            // omitted field would mean "leave it alone".
            ("lorawan.adr=false", ConfigGroup::Lorawan, "2000"),
            ("lorawan.adr=true", ConfigGroup::Lorawan, "2001"),
            (
                "application.interval_report=900",
                ConfigGroup::Application,
                "188407",
            ),
            ("lorawan.region=us915", ConfigGroup::Lorawan, "0801"),
            ("lorawan.region=EU868", ConfigGroup::Lorawan, "0800"),
            ("lorawan.radio_mode=p2p", ConfigGroup::Lorawan, "7802"),
            ("lorawan.radio_mode=lorawan", ConfigGroup::Lorawan, "7801"),
            (
                "sensors.sensor1_rom=28aabbccddeeff11",
                ConfigGroup::Sensors,
                "5a0828aabbccddeeff11",
            ),
            (
                "sensors.accel_motion_sensitivity=high",
                ConfigGroup::Sensors,
                "5003",
            ),
            ("alarms.alarm_limit=4", ConfigGroup::Alarms, "0804"),
            // Field 21 (sticker-firmware 3edb4a4): the tag is two varint bytes.
            ("alarms.radio_alarm_ack=true", ConfigGroup::Alarms, "a80101"),
            (
                "alarms.radio_alarm_ack=false",
                ConfigGroup::Alarms,
                "a80100",
            ),
            // The names before 7be011e's rename land on the same field.
            (
                "lorawan.radio_link_check_interval=6",
                ConfigGroup::Lorawan,
                "6806",
            ),
            (
                "lorawan.radio_link_check_fail_rejoin=5",
                ConfigGroup::Lorawan,
                "7005",
            ),
            (
                "sensors.cap_gpio_mode=inputs",
                ConfigGroup::Sensors,
                "d80101",
            ),
            (
                "sensors.cap_gpio_input_a=pulse",
                ConfigGroup::Sensors,
                "e00103",
            ),
            (
                "sensors.cap_gpio_input_b=analog",
                ConfigGroup::Sensors,
                "e80102",
            ),
            ("p2p.modulation=fsk", ConfigGroup::P2p, "2001"),
            ("p2p.spreading_factor=7", ConfigGroup::P2p, "1007"),
        ];
        // The wire half alone: the `lorawan.*` and `p2p.*` rows are not writable over either
        // radio (see the next tests), which is policy, not encoding.
        for (spec, group, expect) in cases {
            let a = encode_assignment(spec, Transport::P2p).expect(spec);
            assert_eq!(a.group, group, "{spec}");
            assert_eq!(hex(&a.field_bytes), expect, "{spec}");
        }
        // The pre-7be011e spellings and the GP bool caps v2.0.0 reserved are no keys.
        for gone in [
            "lorawan.link_check_interval=6",
            "lorawan.mode=p2p",
            "sensors.cap_input_a=true",
            "sensors.cap_pir_detector=true",
            "sensors.cap_buzzer=true",
        ] {
            assert!(encode_assignment(gone, Transport::P2p).is_err(), "{gone}");
        }
    }

    /// 7be011e's P2P radio tuning is `writable: [shell]`: a downlink on either radio would
    /// sever the link it rides on, so the Hub refuses it before the frame, pointing at the
    /// shell rather than at NFC.
    #[test]
    fn the_p2p_group_is_never_written_over_the_air() {
        for spec in [
            "p2p.frequency=868300000",
            "p2p.spreading_factor=9",
            "p2p.tx_power=10",
        ] {
            for transport in [Transport::Lorawan, Transport::P2p] {
                let err = parse_assignment(spec, transport).expect_err(spec);
                assert_eq!(err.kind(), Kind::InvalidArgument, "{spec}");
                assert!(
                    err.message().contains("not writable over")
                        && err.message().contains("the Node's shell"),
                    "{spec} over {transport}: {}",
                    err.message()
                );
            }
        }
    }

    /// `radio` in 7be011e's `writable:` lists is both radios; the LoRaWAN group is NFC and shell
    /// only on P2P as well.
    #[test]
    fn writability_is_checked_on_the_p2p_radio_too() {
        let err = parse_assignment("lorawan.adr=false", Transport::P2p).expect_err("adr");
        assert!(
            err.message().contains("not writable over P2P") && err.message().contains("NFC"),
            "{}",
            err.message()
        );
        for good in [
            "alarms.alarm_limit=4",
            "application.interval_report=600",
            "sensors.cap_barometer=true",
        ] {
            parse_assignment(good, Transport::P2p).expect(good);
            parse_assignment(good, Transport::Lorawan).expect(good);
        }
    }

    /// `GetParam.p2p_field` is 6, not the fault group 5 the other groups use — 5 is `page`, and
    /// a p2p read sent there would ask for page N of the lorawan-only answer instead.
    #[test]
    fn a_p2p_get_param_asks_in_field_6() {
        let d = Downlink::GetParam {
            fields: vec![(ConfigGroup::Application, 3), (ConfigGroup::P2p, 2)],
        };
        let bytes = d.encode(7, Transport::P2p).expect("encodes");
        // Command { 1 seq=7, 3 get_param { 2 application_field [3], 6 p2p_field [2] } }
        assert_eq!(hex(&bytes), "08071a06120103320102");
    }

    /// What sticker-firmware v2.0.0 would answer `NOT_WRITABLE` or `OUT_OF_RANGE` is refused
    /// before the frame, from its own registry (node_params).
    #[test]
    fn the_firmware_registry_bounds_what_lorawan_may_write() {
        for (bad, why) in [
            ("lorawan.adr=false", "not writable over LoRaWAN"),
            (
                "application.vendor_reset_allow=true",
                "not writable over LoRaWAN",
            ),
            ("application.interval_report=59", "60\u{2013}86400"),
            ("application.interval_report=86401", "60\u{2013}86400"),
            ("application.interval_sample=4", "0 or 5\u{2013}3600"),
            ("application.battery_level=999", "1000\u{2013}3600"),
        ] {
            let err = parse_assignment(bad, Transport::Lorawan).expect_err(bad);
            assert_eq!(err.kind(), Kind::InvalidArgument, "{bad}");
            assert!(err.message().contains(why), "{bad}: {}", err.message());
        }
        for good in [
            "application.interval_report=60",
            "application.interval_sample=0",
            "application.interval_sample=3600",
            "alarms.alarm_buzzer_mode=slow",
            "alarms.radio_alarm_ack=true",
        ] {
            parse_assignment(good, Transport::Lorawan).expect(good);
            parse_assignment(good, Transport::P2p).expect(good);
        }
    }

    /// Key material is settable on the wire and deliberately unspellable here: a `set-config
    /// lorawan.appkey=…` would put a root key in argv, the shell history and `ps` output.
    #[test]
    fn the_write_only_key_material_is_never_settable_from_argv() {
        for name in ["nwkkey", "appkey", "nwkskey", "appskey"] {
            let err = parse_assignment(
                &format!("lorawan.{name}=000102030405060708090a0b0c0d0e0f"),
                Transport::Lorawan,
            )
            .expect_err("refused");
            assert_eq!(err.kind(), Kind::InvalidArgument, "{name}");
            assert!(err.message().contains("ledger #22"), "{name}");
            // And nothing of the key survives into the message.
            assert!(!err.message().contains("000102"), "{name}: leaked");
        }
    }

    #[test]
    fn malformed_assignments_are_refused_before_any_encode() {
        for bad in [
            "not-an-assignment",
            "nosuchgroup.field=1",
            "application.no_such_key=1",
            "application.interval_report=nine hundred",
            "application.interval_report=-1",
            "lorawan.adr=maybe",
            "lorawan.region=mars",
            "sensors.sensor1_rom=28aabb",           // too short
            "sensors.sensor1_rom=zzzzzzzzzzzzzzzz", // not hex
            "alarms.alarm_0=00",                    // the rule is 18 bytes
            // The v1.4 17-byte rule (source/quantity) is not a v2.0.0 rule.
            "alarms.alarm_0=0300000000000070410000c84100000000",
            // v2.0.0 rules the Node would sanitise away (`rule_valid`):
            "alarms.alarm_0=030000020000000020410000f04100000000", // slot 0 takes type 1
            "alarms.alarm_0=030100010000000020410000f04100000000", // s1 typed motherboard
            "alarms.alarm_0=030201020000000020410000f04100000000", // dallas has no ch 1
            "alarms.alarm_0=030114010000000020410000f04100000000", // mb battery, watchdog-only
            "alarms.alarm_0=0300000100000000f0410000204100000000", // hi below lo
            "alarms.alarm_0=030500030000000020410000f04100000000", // slot 5
            "application.history_channels=00a0",                   // slot 5
            "application.history_channels=0000",                   // twice
            "application.history_channels=1f",                     // no mb channel 31
            "sensors.sensor1_type=1",                              // the motherboard
            "sensors.sensor2_type=77",                             // unknown type
            "application.history_sensors=3",                       // retired in v2.0.0
        ] {
            let err = parse_assignment(bad, Transport::Lorawan).expect_err(bad);
            assert_eq!(err.kind(), Kind::InvalidArgument, "{bad}");
            assert_eq!(err.exit_code(), 2, "{bad}");
        }
    }

    /// #119's HIL rule: enabled, s1 machine-probe temperature, lo 10, hi 30, dwell 0. Alarms
    /// field 3 (`alarm_0`), 18 bytes: `1a 12` + the rule.
    #[test]
    fn the_v200_alarm_rule_encodes_as_is() {
        let a = parse_assignment(
            "alarms.alarm_0=030100030000000020410000f04100000000",
            Transport::Lorawan,
        )
        .expect("a valid rule");
        assert_eq!(
            hex(&a.field_bytes),
            "1a12030100030000000020410000f04100000000"
        );
        // An empty value still clears the slot with zero-length bytes.
        let a = parse_assignment("alarms.alarm_0=", Transport::Lorawan).expect("a clear");
        assert_eq!(hex(&a.field_bytes), "1a00");
    }

    /// `history_channels` (application 9) is 24 bytes; a shorter list is the leading columns
    /// and the rest is the firmware's `0xff` fill.
    #[test]
    fn history_channels_pad_with_the_unused_column() {
        let a = parse_assignment("application.history_channels=000120", Transport::Lorawan)
            .expect("mb temperature + humidity, s1 temperature");
        // A 1-Wire column is slot-only: s1 ch 31 is the slot's type's business.
        parse_assignment("application.history_channels=003f", Transport::Lorawan)
            .expect("1-Wire columns are not checked against a type");
        let mut want = "4a18000120".to_string();
        want.push_str(&"ff".repeat(21));
        assert_eq!(hex(&a.field_bytes), want);
    }

    #[test]
    fn the_v200_keys_encode() {
        for (assignment, wire) in [
            ("application.interval_announce=24", "4018"),
            ("application.interval_announce=0", "4000"),
            ("sensors.cap_sht=false", "b00100"),
            ("sensors.sensor1_type=3", "b80103"),
            ("sensors.sensor4_type=2", "d00102"),
            ("sensors.sensor2_type=0", "c00100"),
        ] {
            let a = parse_assignment(assignment, Transport::Lorawan).expect(assignment);
            assert_eq!(hex(&a.field_bytes), wire, "{assignment}");
        }
        assert!(parse_assignment("application.interval_announce=169", Transport::Lorawan).is_err());
    }

    /// §9.7's mandatory guards, reached through the CLI shape rather than through the raw
    /// checkers: an empty `reset-counters` clears everything on the device.
    #[test]
    fn an_empty_reset_counters_never_reaches_the_wire() {
        let args = NodeSendCmdArgs {
            name: "reset-counters".to_string(),
            from: None,
            to: None,
            counter: Vec::new(),
        };
        let err = from_cmd(&args, Transport::Lorawan).expect_err("refused");
        assert_eq!(err.kind(), Kind::InvalidArgument);
        assert!(err.message().contains("every counter"));

        let args = NodeSendCmdArgs {
            name: "reset-counters".to_string(),
            from: None,
            to: None,
            counter: vec![CounterName::HallLeft, CounterName::InputB],
        };
        let d = from_cmd(&args, Transport::Lorawan).expect("selects two counters");
        // 52 04  reset_counters, 4 bytes: hall_left (1) = true, input_b (4) = true
        assert_eq!(encode(&d, 6), "0806520408012001");
    }

    #[test]
    fn a_history_window_is_range_checked_and_ordered() {
        let d = Downlink::ReqHistory {
            from_unix: Some(1_787_000_000),
            to_unix: Some(1_787_000_100),
        };
        assert!(d.encode(1, Transport::Lorawan).is_ok());

        for bad in [
            Downlink::ReqHistory {
                from_unix: Some(1),
                to_unix: None,
            },
            Downlink::ReqHistory {
                from_unix: Some(1_787_000_100),
                to_unix: Some(1_787_000_000),
            },
        ] {
            assert_eq!(
                bad.encode(1, Transport::Lorawan)
                    .expect_err("refused")
                    .kind(),
                Kind::InvalidArgument
            );
        }
    }

    #[test]
    fn arguments_belonging_to_another_command_are_refused_not_ignored() {
        for (name, from, counter) in [
            ("reboot", Some(1_787_000_000i64), Vec::new()),
            ("reboot", None, vec![CounterName::HallLeft]),
            ("history-replay", None, vec![CounterName::HallLeft]),
            (
                "reset-counters",
                Some(1_787_000_000),
                vec![CounterName::HallLeft],
            ),
        ] {
            let args = NodeSendCmdArgs {
                name: name.to_string(),
                from,
                to: None,
                counter,
            };
            let err = from_cmd(&args, Transport::Lorawan).expect_err(name);
            assert_eq!(err.kind(), Kind::InvalidArgument, "{name}");
        }
    }

    /// The vocabulary is one table read from both ends — the property the `ConfigDump`
    /// renderer will depend on.
    #[test]
    fn the_key_table_is_bijective_in_both_directions() {
        for k in CONFIG_KEYS {
            let by_name = lookup(&k.full_name()).expect("looks up by its own name");
            assert_eq!(by_name.field, k.field, "{}", k.full_name());
            let by_wire = key_by_wire(k.group, k.field).expect("looks up by its wire position");
            assert_eq!(by_wire.name, k.name, "{}", k.full_name());
        }
        // No two keys share a wire position.
        let mut seen = std::collections::BTreeSet::new();
        for k in CONFIG_KEYS {
            assert!(
                seen.insert((k.group.fault_group(), k.field)),
                "duplicate wire position for {}",
                k.full_name()
            );
        }
    }

    /// §5's `fault_field` is `group×100 + tag`, so an `Error` naming 203 is
    /// `application.interval_report` — the example the document itself gives.
    #[test]
    fn a_key_reports_the_fault_field_the_device_would_name() {
        let k = lookup("application.interval_report").expect("known key");
        assert_eq!(k.fault_field(), 203);
    }

    #[test]
    fn sequences_continue_from_what_the_cache_recorded() {
        assert_eq!(next_seq(None), 1, "the first command this Hub ever sends");
        assert_eq!(next_seq(Some(7)), 8);
        // Never 0: a response carrying no seq decodes as 0 and must not match.
        assert_eq!(next_seq(Some(u32::MAX)), 1);
    }

    #[test]
    fn get_param_takes_known_keys_only_and_never_key_material() {
        let plan = |keys: &[&str]| {
            super::plan(
                &NodeSendKind::GetParam {
                    params: keys.iter().map(|k| k.to_string()).collect(),
                },
                Transport::Lorawan,
            )
        };
        let ok = plan(&[
            "application.interval_report",
            "alarms.alarm_0",
            "alarms.alarm_0",
        ])
        .expect("known keys plan");
        assert_eq!(ok.label, "get-param");
        assert_eq!(
            ok.downlinks,
            vec![Downlink::GetParam {
                fields: vec![(ConfigGroup::Application, 3), (ConfigGroup::Alarms, 3)]
            }],
            "a repeated key is asked for once"
        );
        let err = plan(&["application.nope"]).expect_err("an unknown key is refused");
        assert_eq!(err.kind(), Kind::InvalidArgument);
        let err = plan(&["lorawan.appkey"]).expect_err("key material is never read");
        assert!(err.message().contains("key material"), "{}", err.message());
    }

    /// The wire has always been able to ask for any page — `ConfigDump` answers exactly the page
    /// requested — but `plan` hard-coded page 0, so a six-page dump could only ever return its
    /// first page and `complete: false` never changed however often the operator re-ran it. The
    /// flag is what makes the rest of the dump reachable (from !57, on both carriers).
    #[test]
    fn get_config_plans_the_page_the_operator_asked_for() {
        for page in [0u32, 1, 5] {
            let p =
                plan(&NodeSendKind::GetConfig { page, seq: None }, Transport::P2p).expect("plans");
            assert_eq!(p.downlinks, vec![Downlink::GetConfig { page }]);
        }
    }
}
