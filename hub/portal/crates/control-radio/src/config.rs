//! The `radio:` configuration section — `plan/control/radio/plan.md` §4.
//!
//! This is one section of the device config (`proximos.config.yaml`), joining `power`,
//! `sensors`, `mqtt`, `ble`, `eye`. YAML, not TOML: TOML in this repo is reserved for the
//! native format of external tools we do not control (`chirpstack.toml`); the application
//! configures *itself* in YAML.
//!
//! **LNS-agnostic by design.** `region`, `mode`, `join_eui`, `application`, `device_profiles`
//! and `gateway` sit directly under `radio:`, not nested under a `chirpstack:` key —
//! `radio.chirpstack` is reserved for whatever turns out to be genuinely
//! ChirpStack-implementation-specific, and is empty today. The API tokens live separately
//! under `/data/proximos/radio/lorawan/` (lorawan.md §4), never in this file.

use std::collections::BTreeMap;

use crate::error::{Component, Error, Kind, Result};
use crate::mode::Mode;

/// Every struct here is `deny_unknown_fields`. That is not tidiness — it is what makes
/// §4's binding invariant structural: `payload_codec_runtime` is **never** a field in this
/// schema, `create` always sends `NONE` to `DeviceProfileService.Create`, and a config that
/// tries to set one fails to parse rather than being silently ignored.
/// `tests/config_schema.rs` pins that.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadioConfig {
    /// v2 physically ships and supports only EU868, so this whole section can be a static
    /// rootfs template default; nothing here needs a `/data`-backed override yet. When
    /// multi-region arrives, only `region` graduates.
    pub region: Region,

    /// Factory choice, set once by the provisioning tool at flash time, never at runtime.
    pub mode: Mode,

    /// Default JoinEUI for `node-add --otaa`, overridable per node with `--join-eui`.
    /// OTAA only — ABP has no join procedure.
    #[serde(default)]
    pub join_eui: Option<String>,

    pub application: ApplicationConfig,

    pub device_profiles: DeviceProfiles,

    /// Hub-wide P2P radio parameters. Present only when a P2P arm is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p2p_radio: Option<P2pRadio>,

    pub gateway: GatewayConfig,

    /// Reserved for genuinely ChirpStack-specific settings; empty today. Typed as an empty
    /// map so a stray key is a parse error rather than silently accepted.
    #[serde(default)]
    pub chirpstack: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Region {
    Eu868,
}

impl Region {
    /// The spelling `proximosctl render` writes into `chirpstack.toml`'s
    /// `[network] enabled_regions`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Region::Eu868 => "eu868",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationConfig {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// Three arms, two different consumers (§4).
///
/// `otaa` and `abp` converge into ChirpStack `DeviceProfile` objects — one per **named**
/// entry, so a fleet defining three OTAA profiles gets three ChirpStack objects. `p2p`
/// **never touches ChirpStack**; it is consumed by the P2P central service.
///
/// Each arm is a named map, and `node-add` requires `--profile <name>` to pick an entry.
/// `default` is the conventional name for a fleet that defines only one.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceProfiles {
    #[serde(default)]
    pub otaa: BTreeMap<String, OtaaProfile>,
    #[serde(default)]
    pub abp: BTreeMap<String, AbpProfile>,
    #[serde(default)]
    pub p2p: BTreeMap<String, P2pProfile>,
}

/// Fields shared by both LoRaWAN activation arms.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OtaaProfile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub mac_version: MacVersion,
    pub reg_params_revision: RegParamsRevision,
    #[serde(default = "default_adr_algorithm")]
    pub adr_algorithm_id: String,
    #[serde(default)]
    pub flush_queue_on_activate: bool,
    /// ChirpStack's inactivity threshold, **not** the device's actual send rate — that is the
    /// separate `application.interval_report` on the Node itself (900 s / 15 min).
    pub uplink_interval: u32,
    pub device_status_req_interval: u32,
    /// Named registry key, not a file path and not a plugin. See `decode::registry`.
    pub decoder: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbpProfile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub mac_version: MacVersion,
    pub reg_params_revision: RegParamsRevision,
    #[serde(default = "default_adr_algorithm")]
    pub adr_algorithm_id: String,
    #[serde(default)]
    pub flush_queue_on_activate: bool,
    pub uplink_interval: u32,
    pub device_status_req_interval: u32,
    /// RX1/RX2 timing, mirroring what the Node firmware sets explicitly
    /// (`node-firmware-v1.4.0.md` §1: RX1 delay 1000 ms, RX2 delay 2000 ms). The DR offset
    /// and RX2 DR/freq are untouched EU868 band defaults.
    pub abp_rx1_delay: u32,
    pub abp_rx1_dr_offset: u32,
    pub abp_rx2_dr: u32,
    pub abp_rx2_freq: u32,
    pub decoder: String,
}

/// Not a ChirpStack object. Additional named p2p profiles may only vary `decoder` — radio
/// parameters are Hub-wide (`p2p_radio`), never per-profile.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct P2pProfile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Same registry, same key — this is the P2P decoder binding (frames 0x02/0x03/0x55).
    pub decoder: String,
}

/// Hub-wide, **not** per-profile (§4).
///
/// The northbridge has exactly one physical channel, so every p2p profile necessarily shares
/// it. Living under `device_profiles.p2p.<name>` would let two profiles imply two channels,
/// which the northbridge cannot do — hoisted out to make that impossible to express.
///
/// The Node firmware carries these values FIXED, so changing any of them without a
/// coordinated firmware release severs the link — the same cross-repo-constant class as the
/// P2P domain-separation labels.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct P2pRadio {
    pub frequency_hz: u32,
    pub spreading_factor: u8,
    pub bandwidth_khz: u32,
    pub coding_rate: String,
    pub preamble: u16,
    /// F5 (decision D3): the TX power in dBm the central assigns to every node in its JoinAccept
    /// `reserved[2]`. `0` — the default, and what an existing config parses as — assigns nothing
    /// and leaves each node on its own `p2p_tx_power`. Otherwise 2..=22, so an existing config
    /// loads; the JoinAccept carries at most 14, the most a node takes (sticker-firmware #486
    /// W8, `control::assigned_node_tx_power`).
    ///
    /// Network-wide rather than per-node: the northbridge has one radio, so SF and channel are
    /// network-wide too (§W2), and power is the one axis a single receiver can vary per device
    /// without a second radio. Per-node assignment is a registry change, not a config one.
    #[serde(default)]
    pub node_tx_power_dbm: u8,
    /// F6: append the 4-byte Unix-time tail to every ACK (the S3 extended form). **On** by
    /// default.
    ///
    /// A P2P node has no wall clock and no `DeviceTimeReq` to ask for one, so this ACK tail is
    /// its only source of time. With it off, every history record decodes with `time: null`, the
    /// portal drops those rows and marks the backfill job DONE — the shipped history feature
    /// inserts nothing at all. Four bytes on an acknowledged uplink is the price of it working,
    /// and the tail is suppressed anyway while the Hub's own clock is unset, so the cost is only
    /// paid where it buys something. An operator who does not want it still says so explicitly.
    /// (The bench host's one-way `--deliver-time` lever went with the host itself in PI1;
    /// this key is the route.)
    #[serde(default = "default_deliver_time")]
    pub deliver_time: bool,
    /// F7: mark a session `RejoinPending` once its uplink counter passes this, so the session key
    /// is replaced before the counter can approach its `u32` saturation (where the node stops
    /// counting and anti-replay stops meaning anything).
    ///
    /// The default is half the counter space, which no real node reaches — a Node reporting every
    /// 15 minutes needs ~61 000 years. It is a config key rather than a constant so the bench can
    /// set it low and actually exercise the rekey path; a `node-rejoin` verb would be the operator
    /// -facing form, and that is an open question for the MR !30 author, not a default.
    #[serde(default = "default_rekey_counter_threshold")]
    pub rekey_counter_threshold: u32,
    /// How long a history replay may go without a frame before the central retires it
    /// (`p2p_history_replay.md` §4.3 option 1, §5.1). The head is popped and the collector
    /// dropped; nothing is published, so the portal's reconciler re-requests the window.
    ///
    /// A config key rather than a constant because §12 leaves the value to the firmware's replay
    /// cadence. The default is an hour: a node's 1 % duty ledger can legitimately block a stream
    /// for about half of one (bench 2026-09-15 measured a 1 801 475 ms TX block), and a shorter
    /// timeout would retire healthy replays.
    #[serde(default = "default_history_stall_timeout_s")]
    pub history_stall_timeout_s: u64,
    /// The modulation (`p2p_tower_gateway.md` §3.5, amended 2026-09-28): `lora`, the only one
    /// until #470 P5 brings `fsk`, which is refused until then.
    #[serde(default)]
    pub modulation: P2pModulation,
    /// The LoRa sync word: `0x12` private (today's) or `0x34` public — the two
    /// `SET_RADIO_CONFIG` encodes, as its `flags` bit 0.
    #[serde(default = "default_sync_word")]
    pub sync_word: u8,
    /// The Northbridge's own TX power in dBm, -16..=15 (RFO_LP). The ACKs, the downlinks and the
    /// JoinAccepts all go out at it; `node_tx_power_dbm` is the Nodes' own.
    #[serde(default = "default_tx_power")]
    pub tx_power: i8,
}

/// `radio.p2p_radio.modulation`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum P2pModulation {
    #[default]
    Lora,
    /// Parsed so the refusal can say what is missing; the modem layer refuses it.
    Fsk,
}

/// The LoRa private sync word — every P2P network so far.
pub const SYNC_WORD_PRIVATE: u8 = 0x12;
/// The LoRa public sync word, LoRaWAN's.
pub const SYNC_WORD_PUBLIC: u8 = 0x34;

pub(crate) const fn default_sync_word() -> u8 {
    SYNC_WORD_PRIVATE
}

pub(crate) const fn default_tx_power() -> i8 {
    14
}

const fn default_rekey_counter_threshold() -> u32 {
    0x7FFF_FFFF
}

pub(crate) const fn default_deliver_time() -> bool {
    true
}

pub(crate) const fn default_history_stall_timeout_s() -> u64 {
    3600
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[allow(non_camel_case_types)]
pub enum MacVersion {
    LORAWAN_1_0_4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[allow(non_camel_case_types)]
pub enum RegParamsRevision {
    RP002_1_0_4,
}

fn default_adr_algorithm() -> String {
    "default".to_string()
}

/// The `radio:` key wrapper, so a whole `proximos.config.yaml` can be parsed for just this
/// section without modelling the rest of the file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeviceConfig {
    pub radio: RadioConfig,
}

impl RadioConfig {
    /// Parse the `radio:` section out of a whole device-config document.
    pub fn from_device_config_yaml(s: &str) -> Result<Self> {
        let doc: DeviceConfig = serde_yaml::from_str(s).map_err(|e| {
            Error::new(
                Kind::InvalidArgument,
                None,
                Component::ConfigJson,
                format!("radio: section is not valid: {e}"),
            )
        })?;
        doc.radio.validate()?;
        Ok(doc.radio)
    }

    /// Semantic checks the type system does not cover.
    pub fn validate(&self) -> Result<()> {
        let invalid = |m: String| Error::new(Kind::InvalidArgument, None, Component::ConfigJson, m);

        // A configured arm must actually have a profile to enrol against, or `node-add
        // --profile` can never succeed and `create` converges nothing.
        match self.mode {
            Mode::Lorawan | Mode::Dual if self.device_profiles.otaa.is_empty() => {
                return Err(invalid(format!(
                    "radio.mode is {} but radio.device_profiles.otaa defines no profile",
                    self.mode.as_str()
                )))
            }
            _ => {}
        }
        match self.mode {
            Mode::P2p | Mode::Dual => {
                if self.device_profiles.p2p.is_empty() {
                    return Err(invalid(format!(
                        "radio.mode is {} but radio.device_profiles.p2p defines no profile",
                        self.mode.as_str()
                    )));
                }
                if self.p2p_radio.is_none() {
                    return Err(invalid(format!(
                        "radio.mode is {} but radio.p2p_radio is absent — the northbridge has \
                         no channel to configure",
                        self.mode.as_str()
                    )));
                }
            }
            _ => {}
        }

        // `chirpstack:` is reserved and empty today. Accepting keys here would create a
        // second, undocumented configuration surface.
        if !self.chirpstack.is_empty() {
            return Err(invalid(
                "radio.chirpstack is reserved and must be empty".to_string(),
            ));
        }

        if let Some(p) = &self.p2p_radio {
            if !(7..=12).contains(&p.spreading_factor) {
                return Err(invalid(format!(
                    "radio.p2p_radio.spreading_factor must be 7..=12, got {}",
                    p.spreading_factor
                )));
            }
            if !matches!(p.bandwidth_khz, 125 | 250 | 500) {
                return Err(invalid(format!(
                    "radio.p2p_radio.bandwidth_khz must be 125, 250 or 500, got {}",
                    p.bandwidth_khz
                )));
            }
            // The Northbridge's own PA: refused here, at load, rather than by 0.3.0 at the
            // first SET_RADIO_CONFIG (`BAD_PARAM`, no clamping).
            if !crate::p2p::modem::NB_TX_POWER_DBM.contains(&p.tx_power) {
                return Err(invalid(format!(
                    "radio.p2p_radio.tx_power must be -16..=15 dBm (the Northbridge's RFO_LP \
                     PA), got {}",
                    p.tx_power
                )));
            }
            // 0 means "assign nothing"; anything else must be a power the node will accept, so a
            // typo is refused here rather than silently ignored by every node in the field.
            if p.node_tx_power_dbm != 0 && !(2..=22).contains(&p.node_tx_power_dbm) {
                return Err(invalid(format!(
                    "radio.p2p_radio.node_tx_power_dbm must be 0 (no assignment) or 2..=22 dBm, \
                     got {}",
                    p.node_tx_power_dbm
                )));
            }
        }
        Ok(())
    }

    /// Resolve `--profile <name>` against the OTAA arm.
    pub fn otaa_profile(&self, name: &str) -> Result<&OtaaProfile> {
        self.device_profiles.otaa.get(name).ok_or_else(|| {
            Error::new(
                Kind::NotFound,
                crate::error::Transport::Lorawan,
                Component::ConfigJson,
                format!(
                    "no OTAA device profile named {name:?}; configured: [{}]",
                    join_keys(self.device_profiles.otaa.keys())
                ),
            )
        })
    }

    pub fn abp_profile(&self, name: &str) -> Result<&AbpProfile> {
        self.device_profiles.abp.get(name).ok_or_else(|| {
            Error::new(
                Kind::NotFound,
                crate::error::Transport::Lorawan,
                Component::ConfigJson,
                format!(
                    "no ABP device profile named {name:?}; configured: [{}]",
                    join_keys(self.device_profiles.abp.keys())
                ),
            )
        })
    }

    pub fn p2p_profile(&self, name: &str) -> Result<&P2pProfile> {
        self.device_profiles.p2p.get(name).ok_or_else(|| {
            Error::new(
                Kind::NotFound,
                crate::error::Transport::P2p,
                Component::ConfigJson,
                format!(
                    "no P2P device profile named {name:?}; configured: [{}]",
                    join_keys(self.device_profiles.p2p.keys())
                ),
            )
        })
    }
}

fn join_keys<'a>(keys: impl Iterator<Item = &'a String>) -> String {
    keys.map(String::as_str).collect::<Vec<_>>().join(", ")
}
