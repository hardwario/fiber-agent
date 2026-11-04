//! The v2 radio gate — decision #12 and `plan/control/radio/plan.md` §1.
//!
//! On a Hub where the v2 radio stack is configured (`radio.mode != off` in the rendered
//! `/run/proximos/control/config.yaml`), `proximos-control` owns the radio: it subscribes
//! ChirpStack's integration events, decodes uplinks and publishes the
//! `proximos/{host}/nodes/…` family, and `proximosctl control.radio` owns enrollment.
//! This crate's own `libs::lorawan` stack — the v1 client, imported wholesale in §11.1 — would
//! then be a **second** MQTT consumer of the same events and a second writer of the same
//! ChirpStack objects, publishing a parallel `{prefix}/{host}/lorawan/sensors` view of the same
//! devices. Two stacks, one radio: whichever wrote last wins, silently.
//!
//! So it is gated off rather than deleted (decision: "gated off behind `radio.mode`, not
//! deleted; delete in a follow-up"). Nothing under `libs/lorawan/` is removed, and a Hub with
//! **no** rendered radio config — every v1-style Hub, and every unit whose renderer has not
//! run — takes the legacy path unchanged. That fallback is the whole reason the decision is
//! expressed as "if the v2 config loads and names a mode" rather than as a build flag.
//!
//! One call site per entry point into the legacy stack. Every one but the first is reachable
//! **without** the monitor, which is why gating the monitor alone was never enough
//! (`plan/control/radio/lorawan.md` §9, amended 2026-10-09):
//!
//! 1. `main.rs`'s `LoRaWANMonitor` construction — the uplink/telemetry consumer.
//! 2. [`crate::libs::lorawan::node_add::add_lorawan_node`] — enrollment, which reaches
//!    `libs::lorawan::provisioning` directly (gRPC-web to ChirpStack): the MQTT
//!    `AddLoRaWANNode` command calls it, and so does the BLE FB0D characteristic on a Hub this
//!    gate leaves alone. On a gated Hub FB0D takes the Node registry path instead (GitLab #118,
//!    [`crate::libs::ble_host::node_enroll`]).
//! 3. [`crate::libs::lorawan::node_remove::remove_lorawan_node`] — the mirror of 2, reaching
//!    `provisioning::deprovision_node`; called by the MQTT `RemoveLoRaWANNode` command, and by
//!    BLE FB12 on a Hub this gate leaves alone, the same way.
//! 4. [`crate::libs::lorawan::external_gateway`] — `add_external_gateway` and
//!    `remove_external_gateway`, reaching `provisioning::{provision,deprovision}_external_gateway`;
//!    called by the MQTT `AddExternalGateway` / `RemoveExternalGateway` commands.
//! 5. [`current_lorawan_units`] — the periodic `system/info` publish, which would otherwise run
//!    `libs::lorawan::detector::detect_gateway()`, a `systemctl is-active` shell-out outside
//!    decision #6's exec set. On a gated Hub the same three booleans come from the v2 stack's
//!    own LoRaWAN document instead.
//!
//! 2–4 refuse before anything is written, with an error that names the `proximosctl
//! control.radio` verb to use instead.

use control_radio::config::RadioConfig;
use control_radio::Mode;

/// Why the legacy stack is disabled, as a log/error line — or `None` when it stays enabled.
///
/// Pure over an already-loaded config so the decision is testable without touching the
/// process-global environment `PROXIMOS_CONFIG` lives in. Both arguments of the decision are
/// visible here: a config that does not load at all (no renderer, v1-style Hub, malformed
/// file) leaves the legacy path **on**, and only an explicit `off` does the same.
pub fn disabled_reason(loaded: &Result<RadioConfig, control_radio::Error>) -> Option<String> {
    match loaded {
        Ok(config) if config.mode != Mode::Off => Some(format!(
            "legacy LoRaWAN stack disabled: radio.mode={} — proximos-control owns the radio",
            config.mode.as_str()
        )),
        // `off` means this Hub has no radio arm at all; the legacy stack is not a fallback for
        // that, it is the other stack, and it must not quietly become the one that runs.
        Ok(_) => None,
        // No rendered config: a v1-style Hub, or one whose renderer has not run yet. The
        // legacy path proceeds exactly as it did before this gate existed.
        Err(_) => None,
    }
}

/// The Hub's default JoinEUI — `radio.join_eui` of an already-loaded config, lowercase —
/// or `None` when no v2 radio config loads or it names none.
///
/// Published as `system/info.lorawan.join_eui` (#86): the portal prefills its Node
/// registration dialog with it, and a blank field there means exactly this value, applied
/// by `enroll_lorawan_node`. Pure for the same reason [`disabled_reason`] is.
pub fn default_join_eui(loaded: &Result<RadioConfig, control_radio::Error>) -> Option<String> {
    loaded
        .as_ref()
        .ok()?
        .join_eui
        .as_deref()
        .map(str::trim)
        .filter(|j| !j.is_empty())
        .map(str::to_ascii_lowercase)
}

/// [`default_join_eui`] against the real rendered config.
pub fn current_default_join_eui() -> Option<String> {
    default_join_eui(&control_radio::dispatch::load_config())
}

/// [`disabled_reason`] against the real rendered config.
///
/// Reads `/run/proximos/control/config.yaml` (`PROXIMOS_CONFIG` overrides it, same convention
/// as `proximosctl`). Called at each entry point rather than resolved once into a global: the
/// file is written by `proximos-render` before any of them runs, the read is one small YAML
/// parse, and a cached answer would be one more thing that can be stale on a Hub whose mode
/// was just re-rendered.
pub fn legacy_lorawan_disabled() -> Option<String> {
    disabled_reason(&control_radio::dispatch::load_config())
}

/// The rendered `radio.mode` as a string (`off`/`lorawan`/`p2p`/`dual`), or `None` when no v2
/// radio config is present (a v1-style Hub, or one whose renderer has not run).
///
/// Published in `system/info.lorawan.mode` so the portal can tell a Hub that *should* have a
/// concentrator (`lorawan`/`dual`) from one where the concentrator's absence is normal
/// (`off`/`p2p`) — the distinction the "concentrator missing" alarm rule keys on. Reads the same
/// rendered `/run/proximos/control/config.yaml` as [`legacy_lorawan_disabled`], with the same
/// "no config → `None`" fallback.
pub fn current_radio_mode() -> Option<String> {
    control_radio::dispatch::load_config()
        .ok()
        .map(|config| config.mode.as_str().to_string())
}

/// This Hub's own concentrator EUI, lowercase, as control-radio's `create` recorded it when it
/// registered the Hub's gateway in its ChirpStack (`gateway_id` of
/// `/data/proximos/radio/lorawan/config.json`) — or `None` when `create` has not converged a
/// gateway (no card, no LoRaWAN arm, a Client Hub with no ChirpStack of its own).
///
/// Published as `system/info.lorawan.gateway_eui`. It replaces the v1 port's
/// `lorawan::cluster::own_gateway_eui`, which scraped `journalctl -u
/// chirpstack-mqtt-forwarder` for a `gateway_id` log line: a shell-out outside decision #6's
/// declared exec set, and an answer that depended on what the journal still held.
pub fn own_gateway_eui(store: &control_radio::store::Store) -> Option<String> {
    let text = store
        .read(control_radio::store::Slot::LorawanConfig)
        .ok()??;
    control_radio::lorawan::ConvergedState::from_json(&text)
        .ok()?
        .gateway_id
        .map(|eui| eui.as_str().to_ascii_lowercase())
}

/// The EUI `system/info.lorawan.gateway_eui` reports: [`own_gateway_eui`] against the shipped
/// radio store, and without a converged one the concentrator's own, asked of concentratord
/// (cluster.md amendment 2026-10-28, #164). A Client runs no ChirpStack, so this is how it
/// reports the EUI its Server's admin grants.
pub fn current_gateway_eui() -> Option<String> {
    own_gateway_eui(&control_radio::store::Store::at_default_root())
        .or_else(crate::libs::lorawan::concentratord::current_gateway_id)
}

/// The three v1 unit booleans of `system/info.lorawan`. `None` is "this Hub cannot say", which
/// the portal reads as unknown (its fields are `Option<bool>`) and never as "down".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LorawanUnits {
    pub gateway_present: Option<bool>,
    pub concentratord_running: Option<bool>,
    pub chirpstack_running: Option<bool>,
}

impl From<crate::libs::lorawan::detector::GatewayDetection> for LorawanUnits {
    fn from(d: crate::libs::lorawan::detector::GatewayDetection) -> Self {
        LorawanUnits {
            gateway_present: Some(d.is_present()),
            concentratord_running: Some(d.concentratord_running),
            chirpstack_running: Some(d.chirpstack_running),
        }
    }
}

/// The LoRaWAN status document (`plan/control/radio/northbridge_diagnostics.md` §4a) as
/// `proximos-control` mirrors it under `/run/proximos/radio/`, beside the retained
/// `radio/lorawan` it publishes.
pub const LORAWAN_RUN_DOCUMENT: &str = "lorawan.json";

/// How many of control-radio's health ticks old that document may be before it stops counting.
/// It is rewritten every tick, so one older than this was left by a `proximos-control` that
/// stopped, and it says nothing about the units any more.
const STALE_AFTER_TICKS: i64 = 3;

/// The unit booleans as the v2 stack's LoRaWAN document reports them. Pure, so every case is
/// testable without a Hub; `max_age_s` bounds how old its `timestamp` may be at `now`.
///
/// A unit is running when the document says its systemd `unit` is `active`, and down in any
/// other state the document names; `unknown` (systemd could not be asked) stays unknown.
/// `gateway_present` keeps v1's meaning — either unit running. A `disabled` document is a mode
/// with no LoRaWAN arm, whose units are condition-skipped by design: all three are `false`, a
/// known answer. No document, or a stale one, is all `None`: a stopped `proximos-control` must
/// not read as a missing concentrator.
pub fn units_from_lorawan_document(
    doc: Option<&serde_json::Value>,
    now: i64,
    max_age_s: i64,
) -> LorawanUnits {
    let Some(doc) = doc else {
        return LorawanUnits::default();
    };
    let fresh = doc
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .is_some_and(|t| now.saturating_sub(t.timestamp()) <= max_age_s);
    if !fresh {
        return LorawanUnits::default();
    }
    if doc.get("state").and_then(|s| s.as_str()) == Some("disabled") {
        return LorawanUnits {
            gateway_present: Some(false),
            concentratord_running: Some(false),
            chirpstack_running: Some(false),
        };
    }
    let running = |component: &str| match doc
        .get(component)
        .and_then(|c| c.get("unit"))
        .and_then(|u| u.as_str())
    {
        None | Some("unknown") => None,
        Some(state) => Some(state == "active"),
    };
    let concentratord_running = running("concentratord");
    let chirpstack_running = running("chirpstack");
    let gateway_present = match (concentratord_running, chirpstack_running) {
        (None, None) => None,
        (c, s) => Some(c == Some(true) || s == Some(true)),
    };
    LorawanUnits {
        gateway_present,
        concentratord_running,
        chirpstack_running,
    }
}

/// `system/info.lorawan`'s unit booleans for this Hub, from whichever stack owns its radio.
///
/// Where the legacy stack runs (no rendered radio config, or `radio.mode: off`) this is v1's
/// `systemctl is-active` probe, unchanged. Where `radio.mode` names a transport it is the v2
/// stack's LoRaWAN document instead: `proximos-control` already asks systemd about the same
/// units every health tick, under northbridge_diagnostics.md §4a, so this process runs no
/// `systemctl` of its own there (decision #6).
pub fn current_lorawan_units() -> LorawanUnits {
    if legacy_lorawan_disabled().is_none() {
        return crate::libs::lorawan::detector::detect_gateway().into();
    }
    let tick_s = control_radio::tasks::health::interval().as_secs() as i64;
    units_from_lorawan_document(
        control_radio::dispatch::read_run_document(LORAWAN_RUN_DOCUMENT).as_ref(),
        control_radio::lorawan::gateway::now_unix(),
        STALE_AFTER_TICKS * tick_s,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The own-EUI source is the converged state's `gateway_id`, never the journal.
    #[test]
    fn the_own_gateway_eui_is_what_create_converged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = control_radio::store::Store::new(dir.path());
        assert_eq!(own_gateway_eui(&store), None, "nothing converged yet");

        std::fs::create_dir_all(dir.path().join("lorawan")).unwrap();
        std::fs::write(
            dir.path().join("lorawan/config.json"),
            r#"{"gateway_id":"0016C001F11B34A6"}"#,
        )
        .unwrap();
        assert_eq!(own_gateway_eui(&store).as_deref(), Some("0016c001f11b34a6"));

        std::fs::write(dir.path().join("lorawan/config.json"), "{}").unwrap();
        assert_eq!(own_gateway_eui(&store), None, "converged without a gateway");
        std::fs::write(dir.path().join("lorawan/config.json"), "not json").unwrap();
        assert_eq!(own_gateway_eui(&store), None, "unreadable is not an EUI");
    }

    /// The `radio:` section as `proximosctl render` writes it, trimmed to what
    /// `RadioConfig::from_device_config_yaml` requires (both arms' profiles, so `dual` also
    /// validates).
    fn config_yaml(mode: &str) -> String {
        format!(
            r#"
radio:
  region: eu868
  mode: {mode}
  application:
    name: "ProXimos Sensors"
  device_profiles:
    otaa:
      default:
        name: "ProXimos Node (OTAA)"
        mac_version: LORAWAN_1_0_4
        reg_params_revision: RP002_1_0_4
        uplink_interval: 3600
        device_status_req_interval: 1
        decoder: "node"
    p2p:
      default:
        name: "ProXimos Node (P2P)"
        decoder: "node"
  p2p_radio:
    frequency_hz: 868100000
    spreading_factor: 10
    bandwidth_khz: 125
    coding_rate: "4/5"
    preamble: 8
  gateway:
    name: "ProXimos Gateway"
"#
        )
    }

    fn loaded(mode: &str) -> Result<RadioConfig, control_radio::Error> {
        RadioConfig::from_device_config_yaml(&config_yaml(mode))
    }

    #[test]
    fn a_configured_v2_radio_disables_the_legacy_stack_and_says_which_mode() {
        for mode in ["lorawan", "p2p", "dual"] {
            let reason = disabled_reason(&loaded(mode))
                .unwrap_or_else(|| panic!("mode {mode} must disable the legacy stack"));
            assert!(reason.contains(&format!("radio.mode={mode}")), "{reason}");
            assert!(
                reason.contains("proximos-control owns the radio"),
                "{reason}"
            );
        }
    }

    #[test]
    fn mode_off_leaves_the_legacy_stack_alone() {
        assert_eq!(disabled_reason(&loaded("off")), None);
    }

    /// The v1-style Hub: no rendered `/run/proximos/control/config.yaml` at all. The legacy
    /// path has to keep working, which is the reason this gate is a config read and not a
    /// build flag.
    #[test]
    fn no_rendered_config_leaves_the_legacy_stack_alone() {
        let missing = RadioConfig::from_device_config_yaml("system: {}");
        assert!(
            missing.is_err(),
            "a file with no radio: section does not load"
        );
        assert_eq!(disabled_reason(&missing), None);

        let malformed = RadioConfig::from_device_config_yaml("radio: [not, a, mapping]");
        assert!(malformed.is_err());
        assert_eq!(disabled_reason(&malformed), None);
    }

    /// #86: the value the portal prefills and a blank registration field falls back to.
    #[test]
    fn the_default_join_eui_is_the_rendered_one_lowercased() {
        let yaml = config_yaml("lorawan").replace(
            "  mode: lorawan\n",
            "  mode: lorawan\n  join_eui: \"58760700000000AB\"\n",
        );
        let loaded = RadioConfig::from_device_config_yaml(&yaml);
        assert_eq!(
            default_join_eui(&loaded).as_deref(),
            Some("58760700000000ab")
        );
    }

    #[test]
    fn no_join_eui_or_no_config_publishes_none() {
        assert_eq!(default_join_eui(&loaded("lorawan")), None);
        assert_eq!(
            default_join_eui(&RadioConfig::from_device_config_yaml("system: {}")),
            None
        );
    }

    // ---------------------------------------------------------------------------------------
    // `system/info.lorawan` on a gated Hub — from the v2 LoRaWAN document, never `systemctl`
    // ---------------------------------------------------------------------------------------

    use control_radio::lorawan::diagnostics as d;

    const NOW: i64 = 1_790_400_000;
    const MAX_AGE: i64 = 180;

    fn unit(active: &str) -> Option<d::Unit> {
        Some(d::Unit {
            active: active.into(),
            sub: String::new(),
            result: String::new(),
            restarts: 0,
        })
    }

    /// The document as control-radio's own producer renders it, so a change to its shape breaks
    /// this test rather than the alarm the portal keys on these fields.
    fn produced(
        concentratord: Option<d::Unit>,
        chirpstack: Option<d::Unit>,
        at: i64,
    ) -> serde_json::Value {
        d::document(&d::Inputs {
            now: at,
            uptime_s: 3600,
            wants_lorawan: true,
            client: false,
            card: d::Card {
                present: true,
                device: d::CARD_TTY.into(),
                usb_id: None,
            },
            concentratord,
            forwarder: unit("active"),
            chirpstack,
            api: Some("ok"),
            observed: Default::default(),
            concentratord_version: None,
            chirpstack_version: None,
        })
    }

    fn units(c: Option<bool>, s: Option<bool>, present: Option<bool>) -> LorawanUnits {
        LorawanUnits {
            gateway_present: present,
            concentratord_running: c,
            chirpstack_running: s,
        }
    }

    #[test]
    fn the_unit_booleans_are_what_the_lorawan_document_says_of_each_unit() {
        let cases = [
            (
                "both up",
                unit("active"),
                unit("active"),
                units(Some(true), Some(true), Some(true)),
            ),
            // v1's `is_present`: ChirpStack alone counts (an external-gateway Hub).
            (
                "no card",
                unit("inactive"),
                unit("active"),
                units(Some(false), Some(true), Some(true)),
            ),
            (
                "all down",
                unit("failed"),
                unit("activating"),
                units(Some(false), Some(false), Some(false)),
            ),
            // systemd could not be asked about one unit: that one is unknown, not down.
            (
                "one unknown",
                None,
                unit("inactive"),
                units(None, Some(false), Some(false)),
            ),
            ("both unknown", None, None, units(None, None, None)),
        ];
        for (label, c, s, want) in cases {
            let doc = produced(c, s, NOW - 30);
            assert_eq!(
                units_from_lorawan_document(Some(&doc), NOW, MAX_AGE),
                want,
                "{label}: {doc}"
            );
        }
    }

    #[test]
    fn a_disabled_arm_is_a_known_false_not_unknown() {
        // p2p: the mode has no LoRaWAN arm and the ChirpStack units are condition-skipped.
        let doc = d::document(&d::Inputs {
            now: NOW - 10,
            uptime_s: u64::MAX,
            wants_lorawan: false,
            client: false,
            card: d::Card {
                present: false,
                device: d::CARD_TTY.into(),
                usb_id: None,
            },
            concentratord: None,
            forwarder: None,
            chirpstack: None,
            api: None,
            observed: Default::default(),
            concentratord_version: None,
            chirpstack_version: None,
        });
        assert_eq!(doc["state"], "disabled");
        assert_eq!(
            units_from_lorawan_document(Some(&doc), NOW, MAX_AGE),
            units(Some(false), Some(false), Some(false))
        );
    }

    #[test]
    fn no_document_or_a_stale_one_is_unknown_never_down() {
        // A stopped proximos-control must not read as "concentrator missing" in the portal.
        assert_eq!(
            units_from_lorawan_document(None, NOW, MAX_AGE),
            LorawanUnits::default()
        );
        let old = produced(unit("inactive"), unit("inactive"), NOW - MAX_AGE - 1);
        assert_eq!(
            units_from_lorawan_document(Some(&old), NOW, MAX_AGE),
            LorawanUnits::default()
        );
        let edge = produced(unit("inactive"), unit("inactive"), NOW - MAX_AGE);
        assert_eq!(
            units_from_lorawan_document(Some(&edge), NOW, MAX_AGE),
            units(Some(false), Some(false), Some(false)),
            "exactly the bound still counts"
        );
        let undated =
            serde_json::json!({ "state": "alive", "concentratord": { "unit": "active" } });
        assert_eq!(
            units_from_lorawan_document(Some(&undated), NOW, MAX_AGE),
            LorawanUnits::default(),
            "a document with no timestamp cannot be shown to be current"
        );
    }

    #[test]
    fn the_legacy_probe_maps_one_to_one() {
        let d = crate::libs::lorawan::detector::GatewayDetection {
            concentratord_running: false,
            chirpstack_running: true,
        };
        assert_eq!(
            LorawanUnits::from(d),
            units(Some(false), Some(true), Some(true))
        );
    }
}
