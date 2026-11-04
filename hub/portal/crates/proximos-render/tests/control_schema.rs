//! The rendered `radio:` section, checked against control-radio's schema — without
//! depending on control-radio.
//!
//! # Why the dependency is not taken
//!
//! `control-radio` may depend on no other crate in this workspace (decision #12,
//! `plan/portal/monolith.md` §11.7), and `tests/standalone.rs` over there enforces it.
//! The reverse edge — this crate depending on control-radio for `RadioConfig` — would not
//! break that rule and would not create a cycle. It is still not taken, for two reasons
//! that outlive the rule:
//!
//! * it would pull the whole tonic/gRPC client, `chirpstack_api`, `rumqttc` and a tokio
//!   runtime into a boot-path renderer that needs none of them, and
//! * the renderer's job is to emit a *document*, not to construct a Rust value. A test
//!   that deserializes the rendered bytes with an independent mirror of the schema
//!   catches a class the shared-type version cannot: a field that serializes and
//!   deserializes fine through one struct but is spelled differently in the file than
//!   control-radio expects.
//!
//! # What keeps the mirror honest
//!
//! Every struct below is a transcription of `crates/control-radio/src/config.rs`,
//! `deny_unknown_fields` included, and that file's own `tests/config_schema.rs` is the
//! authority for it. If the two drift, this test fails on the rendered output — which is
//! the correct place to notice, because the rendered output is the interface.
//!
//! ⚠ **Keep in step with `crates/control-radio/src/config.rs`.**

// The mirror exists to *parse* the rendered document, not to be read from Rust: every
// field below is load-bearing precisely because `deny_unknown_fields` makes its absence a
// deserialization failure. Dead-code analysis cannot see that, so it is turned off for
// the whole file rather than annotated field by field.
#![allow(dead_code)]

mod common;

use std::collections::BTreeMap;

use common::Device;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct DeviceConfigMirror {
    radio: RadioConfigMirror,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RadioConfigMirror {
    region: RegionMirror,
    mode: ModeMirror,
    #[serde(default)]
    join_eui: Option<String>,
    application: NamedMirror,
    device_profiles: DeviceProfilesMirror,
    #[serde(default)]
    p2p_radio: Option<P2pRadioMirror>,
    gateway: NamedMirror,
    #[serde(default)]
    chirpstack: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RegionMirror {
    Eu868,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ModeMirror {
    Off,
    Lorawan,
    P2p,
    Dual,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedMirror {
    name: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceProfilesMirror {
    #[serde(default)]
    otaa: BTreeMap<String, OtaaMirror>,
    #[serde(default)]
    abp: BTreeMap<String, AbpMirror>,
    #[serde(default)]
    p2p: BTreeMap<String, P2pMirror>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OtaaMirror {
    name: String,
    #[serde(default)]
    description: String,
    mac_version: MacVersionMirror,
    reg_params_revision: RegParamsMirror,
    #[serde(default = "default_adr")]
    adr_algorithm_id: String,
    #[serde(default)]
    flush_queue_on_activate: bool,
    uplink_interval: u32,
    device_status_req_interval: u32,
    decoder: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AbpMirror {
    name: String,
    #[serde(default)]
    description: String,
    mac_version: MacVersionMirror,
    reg_params_revision: RegParamsMirror,
    #[serde(default = "default_adr")]
    adr_algorithm_id: String,
    #[serde(default)]
    flush_queue_on_activate: bool,
    uplink_interval: u32,
    device_status_req_interval: u32,
    abp_rx1_delay: u32,
    abp_rx1_dr_offset: u32,
    abp_rx2_dr: u32,
    abp_rx2_freq: u32,
    decoder: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct P2pMirror {
    name: String,
    #[serde(default)]
    description: String,
    decoder: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct P2pRadioMirror {
    frequency_hz: u32,
    spreading_factor: u8,
    bandwidth_khz: u32,
    coding_rate: String,
    preamble: u16,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
enum MacVersionMirror {
    LORAWAN_1_0_4,
}

#[derive(Debug, Deserialize)]
#[allow(non_camel_case_types)]
enum RegParamsMirror {
    RP002_1_0_4,
}

fn default_adr() -> String {
    "default".to_string()
}

fn parse(text: &str) -> DeviceConfigMirror {
    serde_yaml::from_str(text).unwrap_or_else(|e| {
        panic!("the rendered control config is not a valid radio: section: {e}\n{text}")
    })
}

/// Every mode's rendered control config must deserialize cleanly through the mirror. A
/// key control-radio does not model is a startup failure of `proximosctl control.radio`
/// on the device, not a no-op — every struct in that schema is `deny_unknown_fields`.
#[test]
fn every_mode_renders_a_control_config_control_radio_can_parse() {
    for mode in ["off", "lorawan", "p2p", "dual"] {
        let d = Device::new()
            .with_db_key()
            .with_api_secret()
            .with_overlay(&format!("radio:\n  mode: {mode}\n"));
        d.render().unwrap_or_else(|e| panic!("mode {mode}: {e}"));

        let cfg = parse(&d.read_run("control/config.yaml"));
        assert_eq!(cfg.radio.region, RegionMirror::Eu868, "mode {mode}");
        assert_eq!(cfg.radio.mode.as_str_mirror(), mode);
    }
}

impl ModeMirror {
    fn as_str_mirror(&self) -> &'static str {
        match self {
            ModeMirror::Off => "off",
            ModeMirror::Lorawan => "lorawan",
            ModeMirror::P2p => "p2p",
            ModeMirror::Dual => "dual",
        }
    }
}

/// The semantic checks `RadioConfig::validate()` performs on the device, asserted here on
/// the *shipped defaults* so the image cannot ship a config its own consumer rejects.
#[test]
fn the_shipped_defaults_satisfy_control_radios_validate() {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("render");
    let cfg = parse(&d.read_run("control/config.yaml"));

    // "a configured arm must actually have a profile to enrol against"
    assert!(!cfg.radio.device_profiles.otaa.is_empty());
    assert!(!cfg.radio.device_profiles.p2p.is_empty());
    // "the northbridge has no channel to configure" without this
    let p2p = cfg.radio.p2p_radio.as_ref().expect("p2p_radio");
    assert!((7..=12).contains(&p2p.spreading_factor));
    assert!(matches!(p2p.bandwidth_khz, 125 | 250 | 500));
    // "radio.chirpstack is reserved and must be empty"
    assert!(cfg.radio.chirpstack.is_empty());
}

/// A `p2p`/`dual` Hub needs a p2p profile *and* Hub-wide radio parameters, or
/// `validate()` refuses the config. The defaults carry both so that flipping the mode in
/// an overlay is genuinely a one-line change.
#[test]
fn flipping_to_p2p_or_dual_needs_no_second_overlay_edit() {
    for mode in ["p2p", "dual"] {
        let d = Device::new()
            .with_db_key()
            .with_api_secret()
            .with_overlay(&format!("radio:\n  mode: {mode}\n"));
        d.render().unwrap();
        let cfg = parse(&d.read_run("control/config.yaml"));
        assert!(!cfg.radio.device_profiles.p2p.is_empty(), "mode {mode}");
        assert!(cfg.radio.p2p_radio.is_some(), "mode {mode}");
        if mode == "dual" {
            assert!(!cfg.radio.device_profiles.otaa.is_empty());
        }
    }
}

/// `payload_codec_runtime` is never a field in this schema — `create` always sends `NONE`
/// (decision #8). A config that tries to set one must fail to parse rather than be
/// silently ignored, and the renderer must not be the thing that introduces it.
#[test]
fn the_renderer_cannot_introduce_a_payload_codec() {
    let d = Device::new().with_db_key().with_api_secret().with_overlay(
        "radio:\n  device_profiles:\n    otaa:\n      default:\n        \
         payload_codec_runtime: JS\n",
    );
    // The overlay merges — the renderer does not model that key — but control-radio's
    // schema rejects it, which is what the mirror proves.
    d.render().expect("the merge itself does not judge the key");
    let err = serde_yaml::from_str::<DeviceConfigMirror>(&d.read_run("control/config.yaml"))
        .expect_err("control-radio must reject a payload codec");
    assert!(err.to_string().contains("payload_codec_runtime"), "{err}");
}

/// The decoder is a named registry key, and `node` is the one the shipped profiles bind.
#[test]
fn the_shipped_profiles_bind_the_node_decoder() {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("render");
    let cfg = parse(&d.read_run("control/config.yaml"));
    assert_eq!(cfg.radio.device_profiles.otaa["default"].decoder, "node");
    assert!(matches!(
        cfg.radio.device_profiles.otaa["default"].mac_version,
        MacVersionMirror::LORAWAN_1_0_4
    ));
    assert!(matches!(
        cfg.radio.device_profiles.otaa["default"].reg_params_revision,
        RegParamsMirror::RP002_1_0_4
    ));
}

/// The renderer emits `radio:` and `mqtt:`. monolith.md §9.1 has this file eventually
/// carrying a `portal:` section too; until then a *third* top-level key here would be a
/// surprise for whoever reads the file next.
///
/// `mqtt:` arrived with P5 (plan/portal/mqtt/design.md §9.2) and carries exactly one key,
/// `topic_prefix`, emitted from `portal.topic_prefix` so that both halves of
/// `proximos-control` are rendered with one root and cannot be edited apart.
#[test]
fn the_control_config_carries_exactly_two_sections() {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("render");
    let v: serde_yaml::Value = serde_yaml::from_str(&d.read_run("control/config.yaml")).unwrap();
    let keys: Vec<_> = v
        .as_mapping()
        .unwrap()
        .keys()
        .map(|k| k.as_str().unwrap().to_string())
        .collect();
    assert_eq!(keys, vec!["radio", "mqtt"]);

    // And the one key in it is the root both halves publish under — by value, because
    // the portal's own default is what it has to equal.
    assert_eq!(
        v["mqtt"]["topic_prefix"].as_str(),
        Some("proximos"),
        "the rendered root must be the portal's default"
    );
}

/// The Client's broker override (plan/control/cluster.md §4.4) lives under `mqtt:`, which
/// control-radio does not model, so the `radio:` section control-radio reads is the
/// Server's, byte for byte, and the file still carries exactly two sections.
#[test]
fn a_client_control_config_is_still_one_control_radio_can_parse() {
    let server = Device::new().with_db_key().with_api_secret();
    server.render().expect("render");
    let client = Device::new()
        .with_db_key()
        .with_api_secret()
        .joined_as_client();
    client.render().expect("render");

    let text = client.read_run("control/config.yaml");
    let cfg = parse(&text);
    assert_eq!(cfg.radio.mode.as_str_mirror(), "lorawan");

    let radio = |t: &str| -> serde_yaml::Value {
        serde_yaml::from_str::<serde_yaml::Value>(t).unwrap()["radio"].clone()
    };
    assert_eq!(radio(&text), radio(&server.read_run("control/config.yaml")));

    let v: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
    let keys: Vec<_> = v
        .as_mapping()
        .unwrap()
        .keys()
        .map(|k| k.as_str().unwrap().to_string())
        .collect();
    assert_eq!(keys, vec!["radio", "mqtt"]);
}
