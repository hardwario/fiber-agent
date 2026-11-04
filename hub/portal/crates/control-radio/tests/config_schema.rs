//! The `radio:` schema against `plan/control/radio/plan.md` §4.
//!
//! The fixture is the plan's own example block, extracted verbatim: if the schema cannot
//! parse the document's own YAML, the schema is wrong, not the document.

use control_radio::config::{RadioConfig, Region};
use control_radio::{Kind, Mode};

const PLAN_EXAMPLE: &str = include_str!("fixtures/radio-section.yaml");

#[test]
fn the_plans_own_example_parses() {
    let cfg = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE)
        .expect("plan.md §4's example block must parse");

    assert_eq!(cfg.region, Region::Eu868);
    assert_eq!(cfg.mode, Mode::Lorawan);
    assert_eq!(cfg.join_eui.as_deref(), Some("5876070000000000"));
    assert_eq!(cfg.application.name, "ProXimos Sensors");
    assert_eq!(cfg.gateway.name, "ProXimos Gateway");

    // Each arm is a named map, not a single object.
    assert_eq!(cfg.device_profiles.otaa.len(), 1);
    assert_eq!(cfg.device_profiles.abp.len(), 1);
    assert_eq!(cfg.device_profiles.p2p.len(), 1);
    assert_eq!(
        cfg.otaa_profile("default").expect("default otaa").name,
        "ProXimos Node (OTAA)"
    );
    assert_eq!(
        cfg.abp_profile("default").expect("default abp").name,
        "ProXimos Node (ABP)"
    );
    assert_eq!(
        cfg.p2p_profile("default").expect("default p2p").name,
        "ProXimos Node (P2P)"
    );

    // The decoder is a named registry key on every arm — no transport left with an implicit
    // default (§4).
    assert_eq!(cfg.otaa_profile("default").expect("otaa").decoder, "node");
    assert_eq!(cfg.abp_profile("default").expect("abp").decoder, "node");
    assert_eq!(cfg.p2p_profile("default").expect("p2p").decoder, "node");

    // p2p_radio is hoisted Hub-wide, never per-profile: the northbridge has one channel.
    let p2p_radio = cfg
        .p2p_radio
        .as_ref()
        .expect("p2p_radio present in the example");
    assert_eq!(p2p_radio.frequency_hz, 868_100_000);
    assert_eq!(p2p_radio.spreading_factor, 10);
    assert_eq!(p2p_radio.bandwidth_khz, 125);

    // Reserved and empty today.
    assert!(cfg.chirpstack.is_empty());
}

/// §4's binding invariant: `payload_codec_runtime` is **never** a field in this schema.
/// `create` always sends `NONE` to `DeviceProfileService.Create` regardless of anything in
/// the file — so a config that tries to set one must fail to parse, not be silently ignored.
#[test]
fn payload_codec_runtime_is_not_expressible() {
    for injected in [
        "payload_codec_runtime: JS",
        "payload_codec_script: \"function decodeUplink() {}\"",
    ] {
        let mut doc = String::new();
        for line in PLAN_EXAMPLE.lines() {
            doc.push_str(line);
            doc.push('\n');
            if line
                .trim_start()
                .starts_with("name: \"ProXimos Node (OTAA)\"")
            {
                // Same indentation as the sibling key, so the injected line is a real
                // member of the profile map rather than a YAML syntax error.
                let indent = &line[..line.len() - line.trim_start().len()];
                doc.push_str(indent);
                doc.push_str(injected);
                doc.push('\n');
            }
        }
        let err = RadioConfig::from_device_config_yaml(&doc)
            .expect_err("a payload codec key must be a hard parse error");
        assert_eq!(err.kind(), Kind::InvalidArgument);
        assert!(
            err.message().contains("unknown field"),
            "expected an unknown-field rejection, got: {}",
            err.message()
        );
    }
}

/// Any unknown key is rejected, not just the codec ones — that is what makes the invariant
/// structural rather than a special case.
#[test]
fn unknown_keys_are_rejected_everywhere() {
    let doc = PLAN_EXAMPLE.replace("  region: eu868", "  region: eu868\n  reggion: eu868");
    let err = RadioConfig::from_device_config_yaml(&doc).expect_err("typo must not be ignored");
    assert_eq!(err.kind(), Kind::InvalidArgument);
}

#[test]
fn a_configured_arm_must_have_a_profile_to_enrol_against() {
    // mode: p2p with no p2p profile is unusable — node-add --profile can never succeed.
    let doc = PLAN_EXAMPLE.replace("  mode: lorawan", "  mode: p2p");
    let cfg = RadioConfig::from_device_config_yaml(&doc);
    assert!(
        cfg.is_ok(),
        "the example does define a p2p profile and p2p_radio"
    );

    let stripped = strip_block(PLAN_EXAMPLE, "    p2p:", "  p2p_radio:");
    let doc = stripped.replace("  mode: lorawan", "  mode: p2p");
    let err = RadioConfig::from_device_config_yaml(&doc)
        .expect_err("mode p2p with no p2p profile must be refused");
    assert_eq!(err.kind(), Kind::InvalidArgument);
    assert!(err.message().contains("device_profiles.p2p"));
}

#[test]
fn p2p_mode_without_radio_parameters_is_refused() {
    let doc = strip_block(PLAN_EXAMPLE, "  p2p_radio:", "  gateway:")
        .replace("  mode: lorawan", "  mode: p2p");
    let err = RadioConfig::from_device_config_yaml(&doc)
        .expect_err("mode p2p with no p2p_radio must be refused");
    assert_eq!(err.kind(), Kind::InvalidArgument);
    assert!(err.message().contains("p2p_radio"));
}

/// F5 (decision D3): the TX power the central hands every node in its JoinAccept. 0 is "assign
/// nothing"; anything else must be a power the node will actually accept, so a typo is refused
/// at load rather than silently ignored by every node in the field.
#[test]
fn the_assigned_node_tx_power_must_be_zero_or_a_power_the_node_accepts() {
    for good in ["0", "2", "14", "22"] {
        let doc = PLAN_EXAMPLE.replace(
            "    node_tx_power_dbm: 0",
            &format!("    node_tx_power_dbm: {good}"),
        );
        let cfg = RadioConfig::from_device_config_yaml(&doc)
            .unwrap_or_else(|e| panic!("{good} dBm should parse: {e}"));
        assert_eq!(
            cfg.p2p_radio.as_ref().expect("p2p_radio").node_tx_power_dbm,
            good.parse::<u8>().unwrap()
        );
    }
    // 1 is below the node's floor and 23 above its ceiling; both are refused by name.
    for bad in ["1", "23", "255"] {
        let doc = PLAN_EXAMPLE.replace(
            "    node_tx_power_dbm: 0",
            &format!("    node_tx_power_dbm: {bad}"),
        );
        let err = RadioConfig::from_device_config_yaml(&doc)
            .expect_err(&format!("{bad} dBm must be refused"));
        assert_eq!(err.kind(), Kind::InvalidArgument);
        assert!(
            err.message().contains("node_tx_power_dbm"),
            "the message names the key: {}",
            err.message()
        );
    }
}

/// F7: the rekey threshold defaults to half the counter space — no real node reaches it, so the
/// default never fires — and is a config key so the bench can set it low and exercise the path.
#[test]
fn the_rekey_threshold_defaults_high_and_is_settable() {
    let cfg = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("the example parses");
    assert_eq!(
        cfg.p2p_radio
            .as_ref()
            .expect("p2p_radio")
            .rekey_counter_threshold,
        0x7FFF_FFFF,
        "absent means half the counter space, not zero — zero would rekey every session at once"
    );

    let low = PLAN_EXAMPLE.replace(
        "    deliver_time: true",
        "    deliver_time: true\n    rekey_counter_threshold: 2",
    );
    let cfg = RadioConfig::from_device_config_yaml(&low).expect("an explicit threshold parses");
    assert_eq!(
        cfg.p2p_radio
            .as_ref()
            .expect("p2p_radio")
            .rekey_counter_threshold,
        2
    );
}

/// F6: time delivery is a configuration key, not only a bench flag, and it defaults **on**.
///
/// With it off, a P2P node has no wall clock and no `DeviceTimeReq` to ask for one, so every
/// history record decodes with `time: null`; the portal drops those rows and marks the backfill
/// job DONE, and the shipped history feature inserts nothing at all. Four bytes on an
/// acknowledged uplink is the price, and the tail is suppressed anyway while the Hub's own clock
/// is unset — so the default that makes the feature work is the right one.
#[test]
fn time_delivery_is_on_unless_the_config_turns_it_off() {
    let cfg = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("the example parses");
    let p2p = cfg.p2p_radio.as_ref().expect("p2p_radio");
    assert!(p2p.deliver_time, "the example ships it on");
    assert_eq!(p2p.node_tx_power_dbm, 0, "and assigns no power");

    let off = PLAN_EXAMPLE.replace("    deliver_time: true", "    deliver_time: false");
    let cfg = RadioConfig::from_device_config_yaml(&off).expect("deliver_time: false parses");
    assert!(
        !cfg.p2p_radio.as_ref().expect("p2p_radio").deliver_time,
        "an operator who does not want the tail can still say so"
    );

    // Both keys absent — a config written before F5/F6 — still loads, on today's defaults.
    let older = strip_block(PLAN_EXAMPLE, "    node_tx_power_dbm: 0", "  gateway:");
    let cfg = RadioConfig::from_device_config_yaml(&older).expect("a pre-F5/F6 config still loads");
    let p2p = cfg.p2p_radio.as_ref().expect("p2p_radio");
    assert_eq!(p2p.node_tx_power_dbm, 0);
    assert!(p2p.deliver_time, "absent now means on");
}

/// The history inter-frame timeout is a config key rather than a constant: `p2p_history_replay.md`
/// §12 leaves the value to the firmware's replay cadence, and a node's duty ledger can legitimately
/// block a stream for ~30 minutes. Absent means an hour, so a `p2p_radio:` block written before the
/// key existed still parses and still retires nothing early.
#[test]
fn the_history_stall_timeout_defaults_to_an_hour_and_is_settable() {
    let cfg = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("the example parses");
    assert_eq!(
        cfg.p2p_radio
            .as_ref()
            .expect("p2p_radio")
            .history_stall_timeout_s,
        3600
    );

    let shorter = PLAN_EXAMPLE.replace(
        "    history_stall_timeout_s: 3600",
        "    history_stall_timeout_s: 90",
    );
    let cfg = RadioConfig::from_device_config_yaml(&shorter).expect("an explicit value parses");
    assert_eq!(
        cfg.p2p_radio
            .as_ref()
            .expect("p2p_radio")
            .history_stall_timeout_s,
        90
    );

    // The key absent entirely — a config written before it existed — still loads on the default.
    let older = strip_block(
        PLAN_EXAMPLE,
        "    history_stall_timeout_s: 3600",
        "  gateway:",
    );
    let cfg = RadioConfig::from_device_config_yaml(&older).expect("a pre-key config still loads");
    assert_eq!(
        cfg.p2p_radio
            .as_ref()
            .expect("p2p_radio")
            .history_stall_timeout_s,
        3600
    );
}

/// The three keys of `p2p_tower_gateway.md` §3.5 (amended 2026-09-28): parsed as the example
/// states them, and absent — every config written before them — on the same defaults.
#[test]
fn modulation_sync_word_and_tx_power_parse_and_default() {
    let cfg = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("the example parses");
    let p2p = cfg.p2p_radio.as_ref().expect("p2p_radio");
    assert_eq!(p2p.modulation, control_radio::config::P2pModulation::Lora);
    assert_eq!(p2p.sync_word, 0x12);
    assert_eq!(p2p.tx_power, 14);

    let public = PLAN_EXAMPLE
        .replace("    sync_word: 0x12", "    sync_word: 0x34")
        .replace("    modulation: lora", "    modulation: fsk");
    let cfg = RadioConfig::from_device_config_yaml(&public).expect("parses");
    let p2p = cfg.p2p_radio.as_ref().expect("p2p_radio");
    assert_eq!(p2p.sync_word, 0x34);
    // Parsed, so the modem layer can say why it refuses it.
    assert_eq!(p2p.modulation, control_radio::config::P2pModulation::Fsk);

    let older = strip_block(PLAN_EXAMPLE, "    modulation: lora", "  gateway:");
    let cfg = RadioConfig::from_device_config_yaml(&older).expect("a pre-key config still loads");
    let p2p = cfg.p2p_radio.as_ref().expect("p2p_radio");
    assert_eq!(p2p.modulation, control_radio::config::P2pModulation::Lora);
    assert_eq!(p2p.sync_word, 0x12);
    assert_eq!(p2p.tx_power, 14);
}

/// The Northbridge's RFO_LP PA runs -16..=15 dBm and 0.3.0 refuses the rest without clamping,
/// so the config is refused at load, before any `SET_RADIO_CONFIG`.
#[test]
fn the_northbridge_tx_power_is_its_pa_range() {
    let with = |p: &str| PLAN_EXAMPLE.replace("    tx_power: 14 ", &format!("    tx_power: {p} "));
    for ok in ["-16", "15"] {
        let cfg = RadioConfig::from_device_config_yaml(&with(ok)).expect("in range");
        assert!(cfg.validate().is_ok(), "{ok}");
    }
    for bad in ["-17", "16", "22"] {
        let refused = RadioConfig::from_device_config_yaml(&with(bad)).and_then(|c| c.validate());
        assert_eq!(
            refused.expect_err(bad).kind(),
            Kind::InvalidArgument,
            "{bad}"
        );
    }
}

#[test]
fn chirpstack_is_reserved_and_must_stay_empty() {
    let doc = PLAN_EXAMPLE.replace(
        "  chirpstack: {}",
        "  chirpstack:\n    api_bind: \"0.0.0.0:8080\"",
    );
    let err = RadioConfig::from_device_config_yaml(&doc).expect_err("radio.chirpstack is reserved");
    assert_eq!(err.kind(), Kind::InvalidArgument);
}

#[test]
fn an_unknown_profile_name_is_not_found_not_a_panic() {
    let cfg = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("parses");
    let err = cfg
        .otaa_profile("does-not-exist")
        .expect_err("must be NotFound");
    assert_eq!(err.kind(), Kind::NotFound);
    // The message names what *is* configured, so the operator can fix the flag.
    assert!(err.message().contains("default"));
}

/// Drop the lines from `start` (inclusive) up to `end` (exclusive), both matched by prefix.
fn strip_block(src: &str, start: &str, end: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in src.lines() {
        if line.starts_with(start) {
            skipping = true;
        } else if line.starts_with(end) {
            skipping = false;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}
