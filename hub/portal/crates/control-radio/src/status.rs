//! `status --json` and the command result envelope — `plan/control/radio/plan.md` §5.
//!
//! These shapes are a **golden baseline**, not cosmetics: the provisioning wizard and the fleet
//! updater consume them over SSH. Evolution is additive-only, exactly like the exit codes — a
//! new key may appear, an existing one never changes meaning or disappears.

use crate::lorawan::converge::LorawanReport;
use crate::mode::Mode;

/// Whether the process hosting this component's loop duties is up.
///
/// The plan gave these duties to a resident `control-radio.service`, so the key is named
/// `daemon`. Decision #12 dissolves that unit — the ChirpStack event subscription, the 60 s
/// health poll and the `node-get` cache become tasks inside `proximos-control`. The key is kept
/// because it is a consumed contract, and it now answers the question it always meant: is
/// anything running those loops?
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DaemonStatus {
    pub running: bool,
    pub version: String,
}

impl DaemonStatus {
    pub fn new(running: bool) -> Self {
        DaemonStatus {
            running,
            version: crate::VERSION.to_string(),
        }
    }
}

/// The `p2p` block. Deliberately has **no `drift` key**: `converged`/`drift` describe
/// reconciling against ChirpStack's own state, and the P2P arm has no external system to
/// diverge from — only its own local registry and session DB.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct P2pReport {
    pub initialized: bool,
    /// Mirrors the per-arm boot marker: the northbridge probe succeeded.
    pub provisioned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net_id: Option<String>,
    pub session_db: &'static str,
    /// What spreading factor is actually in force, and whether `radio-set` moved it. `None` only
    /// when the Hub config carries no `radio.p2p_radio` block to compare against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spreading_factor: Option<SpreadingFactorReport>,
    /// The northbridge status document `proximos-control` last wrote
    /// (`northbridge_diagnostics.md` §3), absent when the daemon has not written one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub northbridge: Option<serde_json::Value>,
}

/// The config key, the value in force, and whether the two differ.
///
/// Three fields rather than one, because "what did I set" and "what is the Hub running" are
/// different questions and an operator who has just run `radio-set` needs both: the override
/// lives in `sessions.db` and the config key in the rendered config, and neither alone says
/// whether the lever is engaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SpreadingFactorReport {
    pub config: u8,
    pub effective: u8,
    pub overridden: bool,
}

impl SpreadingFactorReport {
    /// Build from the config key and the `radio-set` override, if any.
    pub fn from_parts(config: u8, override_sf: Option<u8>) -> Self {
        SpreadingFactorReport {
            config,
            effective: override_sf.unwrap_or(config),
            overridden: override_sf.is_some(),
        }
    }
}

/// The whole `status --json` document. Transport keys are present per `radio.mode`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct StatusReport {
    pub radio_mode: &'static str,
    pub daemon: DaemonStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lorawan: Option<LorawanReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p2p: Option<P2pReport>,
}

impl StatusReport {
    pub fn new(mode: Mode, daemon_running: bool) -> Self {
        StatusReport {
            radio_mode: mode.as_str(),
            daemon: DaemonStatus::new(daemon_running),
            lorawan: None,
            p2p: None,
        }
    }
}

/// The success envelope every **mutating** command returns — §5.
///
/// One shape regardless of `--radio` or activation mode. On failure there is no second shape
/// either: it is the §6 error envelope. No command invents its own format.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CommandResult {
    pub success: bool,
    pub command: &'static str,
    pub radio: &'static str,
    /// The command-specific fields, flattened alongside the common three.
    #[serde(flatten)]
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl CommandResult {
    pub fn new(command: &'static str, radio: crate::Transport) -> Self {
        CommandResult {
            success: true,
            command,
            radio: radio.as_str(),
            fields: serde_json::Map::new(),
        }
    }

    pub fn with(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        self.fields.insert(key.to_string(), value.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lorawan_only_hub_omits_the_p2p_block() {
        let s = StatusReport::new(Mode::Lorawan, true);
        let json = serde_json::to_value(&s).expect("serializes");
        assert_eq!(json["radio_mode"], "lorawan");
        assert!(
            json.get("p2p").is_none(),
            "transport keys are present per radio.mode"
        );
        assert_eq!(json["daemon"]["running"], true);
    }

    /// The p2p block carries no `drift`: there is no external system for it to diverge from.
    /// It also carries no `pairing`: the window concept is withdrawn (`p2p.md` §8), so the
    /// block is exactly init/provisioned state, `net_id` and session-DB health.
    #[test]
    fn the_p2p_block_has_no_drift_key() {
        let p = P2pReport {
            initialized: true,
            provisioned: false,
            net_id: Some("a1b2c3d4".into()),
            session_db: "ok",
            spreading_factor: None,
            northbridge: None,
        };
        let json = serde_json::to_value(&p).expect("serializes");
        assert!(json.get("drift").is_none());
        assert!(json.get("converged").is_none());
        assert!(json.get("pairing").is_none());
        // But it does mirror the per-arm boot marker, like the lorawan block.
        assert_eq!(json["provisioned"], false);
        assert_eq!(json["net_id"], "a1b2c3d4");
        assert_eq!(json["session_db"], "ok");
    }

    /// The envelope is one shape: three common fields plus command-specific ones alongside
    /// them, never nested under a per-command key.
    #[test]
    fn the_command_envelope_is_one_shape() {
        let r = CommandResult::new("node-add", crate::Transport::Lorawan)
            .with("serial_number", "SN-1")
            .with("dev_eui", "0011223344556677")
            .with("profile", "default")
            .with("activation", "otaa");
        let json = serde_json::to_value(&r).expect("serializes");
        assert_eq!(json["success"], true);
        assert_eq!(json["command"], "node-add");
        assert_eq!(json["radio"], "lorawan");
        assert_eq!(json["serial_number"], "SN-1");
        assert_eq!(json["activation"], "otaa");
        // Flattened, not nested.
        assert!(json.get("fields").is_none());
    }

    /// §5's P2P `node-add` is registration, and the bundle is required — so the identity is
    /// always present and there is no `pairing_window` field to carry. The window-only variant
    /// this test used to pin (identity absent, `pairing_window` present) no longer exists.
    #[test]
    fn a_p2p_add_always_carries_the_identity_and_never_a_window() {
        let r = CommandResult::new("node-add", crate::Transport::P2p)
            .with("profile", "default")
            .with("serial_number", "SN125")
            .with("dev_eui", "70b3d57ed0000abe");
        let json = serde_json::to_value(&r).expect("serializes");
        assert_eq!(json["command"], "node-add");
        assert_eq!(json["radio"], "p2p");
        assert_eq!(json["serial_number"], "SN125");
        assert_eq!(json["dev_eui"], "70b3d57ed0000abe");
        assert_eq!(json["profile"], "default");
        assert!(json.get("pairing_window").is_none());
        // The one LoRaWAN-only field stays LoRaWAN-only.
        assert!(json.get("activation").is_none());
    }

    /// `radio-set` is only useful if `status` says what is actually in force: the config key,
    /// the effective value, and whether the two differ because an operator moved it.
    #[test]
    fn the_p2p_block_reports_config_and_effective_spreading_factor() {
        let mut p = P2pReport {
            initialized: true,
            provisioned: true,
            net_id: Some("a1b2c3d4".into()),
            session_db: "ok",
            spreading_factor: Some(SpreadingFactorReport {
                config: 10,
                effective: 9,
                overridden: true,
            }),
            northbridge: None,
        };
        let json = serde_json::to_value(&p).expect("serializes");
        assert_eq!(json["spreading_factor"]["config"], 10);
        assert_eq!(json["spreading_factor"]["effective"], 9);
        assert_eq!(json["spreading_factor"]["overridden"], true);

        // With no override the two agree and nothing is flagged — the config key is the truth.
        p.spreading_factor = Some(SpreadingFactorReport::from_parts(10, None));
        let json = serde_json::to_value(&p).expect("serializes");
        assert_eq!(json["spreading_factor"]["config"], 10);
        assert_eq!(json["spreading_factor"]["effective"], 10);
        assert_eq!(json["spreading_factor"]["overridden"], false);
        assert_eq!(
            SpreadingFactorReport::from_parts(10, Some(12)),
            SpreadingFactorReport {
                config: 10,
                effective: 12,
                overridden: true,
            }
        );
    }

    /// A dual-mode Hub carries both blocks.
    #[test]
    fn dual_mode_carries_both_transport_blocks() {
        let mut s = StatusReport::new(Mode::Dual, true);
        s.p2p = Some(P2pReport {
            initialized: true,
            provisioned: true,
            net_id: Some("a1b2c3d4".into()),
            session_db: "ok",
            spreading_factor: None,
            northbridge: None,
        });
        let json = serde_json::to_value(&s).expect("serializes");
        assert_eq!(json["radio_mode"], "dual");
        assert!(json.get("p2p").is_some());
    }

    #[test]
    fn the_daemon_version_is_the_build_version() {
        let d = DaemonStatus::new(false);
        assert_eq!(d.version, crate::VERSION);
        assert!(!d.running);
    }
}
