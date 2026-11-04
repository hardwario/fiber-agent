//! Golden-file tests: what one render actually puts on a device, per `radio.mode`.
//!
//! The goldens are the review surface. A template edit that changes a rendered byte shows
//! up here as a diff in the same commit, which is the only mechanism that keeps
//! `[api] bind = "127.0.0.1:8080"` from quietly becoming `0.0.0.0` again.

mod common;

use common::{assert_golden, Device};

const PORTAL: &str = "portal/config.yaml";
const CONTROL: &str = "control/config.yaml";
const CHIRPSTACK: &str = "chirpstack/chirpstack.toml";
const REGION: &str = "chirpstack/region_eu868.toml";
const FORWARDER: &str = "chirpstack-mqtt-forwarder/chirpstack-mqtt-forwarder.toml";
const MOSQUITTO: &str = "mosquitto/8883.conf";

/// The default of a Hub with the RAK5146 and nothing chosen: `radio.mode: lorawan`
/// (node_registry.md §12), a device that has run its migrations, and a Server (no role
/// marker). Every output (six files — ChirpStack takes two).
#[test]
fn lorawan_renders_every_output() {
    let d = Device::new().with_db_key().with_api_secret();
    let report = d.render().expect("render");

    assert_eq!(
        d.run_tree(),
        vec![
            FORWARDER.to_string(),
            CHIRPSTACK.to_string(),
            REGION.to_string(),
            CONTROL.to_string(),
            MOSQUITTO.to_string(),
            PORTAL.to_string(),
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>(),
    );
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert!(report.removed.is_empty(), "{:?}", report.removed);

    assert_golden("lorawan/portal-config.yaml", &d.read_run(PORTAL));
    assert_golden("lorawan/control-config.yaml", &d.read_run(CONTROL));
    assert_golden("lorawan/chirpstack.toml", &d.read_run(CHIRPSTACK));
    assert_golden("lorawan/region_eu868.toml", &d.read_run(REGION));
    assert_golden("lorawan/forwarder.toml", &d.read_run(FORWARDER));
    assert_golden("server/mosquitto-8883.conf", &d.read_run(MOSQUITTO));
}

/// `dual` is `lorawan` plus a P2P arm, so the server half is configured identically —
/// asserted against the same goldens rather than a copy of them, so the two can never
/// drift apart silently.
#[test]
fn dual_renders_the_same_server_half_as_lorawan() {
    let d = Device::new()
        .with_db_key()
        .with_api_secret()
        .with_overlay("radio:\n  mode: dual\n");
    d.render().expect("render");

    assert_golden("lorawan/chirpstack.toml", &d.read_run(CHIRPSTACK));
    assert_golden("lorawan/region_eu868.toml", &d.read_run(REGION));
    assert_golden("lorawan/forwarder.toml", &d.read_run(FORWARDER));
    assert_golden("lorawan/portal-config.yaml", &d.read_run(PORTAL));
    // The control config differs in exactly one line, and that is the whole point of it.
    assert!(d.read_run(CONTROL).contains("mode: dual"));
}

/// `off` and `p2p` get the two unconditional outputs and nothing else. The absence of the
/// other three is the service gate every consuming unit's `ConditionPathExists=` reads.
#[test]
fn off_and_p2p_render_only_the_portal_and_control_configs() {
    for mode in ["off", "p2p"] {
        let d = Device::new()
            .with_db_key()
            .with_api_secret()
            .with_overlay(&format!("radio:\n  mode: {mode}\n"));
        let report = d.render().unwrap_or_else(|e| panic!("mode {mode}: {e}"));

        assert!(d.exists(PORTAL), "mode {mode}: portal config");
        assert!(d.exists(CONTROL), "mode {mode}: control config");
        assert!(!d.exists(CHIRPSTACK), "mode {mode}: chirpstack.toml");
        assert!(!d.exists(REGION), "mode {mode}: region file");
        assert!(!d.exists(FORWARDER), "mode {mode}: forwarder toml");
        // The radio mode is not the broker's business: a P2P or radio-off Server takes
        // Clients too (cluster.md §2), so its 8883 listener is rendered all the same.
        assert_golden("server/mosquitto-8883.conf", &d.read_run(MOSQUITTO));

        // Skipped, not silently omitted: the reason travels to the operator.
        assert_eq!(report.skipped.len(), 2, "mode {mode}: {:?}", report.skipped);
        for s in &report.skipped {
            assert!(s.reason.contains(mode), "mode {mode}: {}", s.reason);
        }
        // And the control config still names the mode, so control-radio can answer with
        // OffMode / P2PUnavailable rather than NotBootstrapped.
        assert!(d.read_run(CONTROL).contains(&format!("mode: {mode}")));
    }
}

/// The mode flip. A Hub configured for LoRaWAN, re-provisioned to P2P, must not keep a
/// live ChirpStack config on the tmpfs — `ConditionPathExists=` would read it as consent.
#[test]
fn flipping_the_mode_away_from_lorawan_removes_the_stale_configs() {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("first render");
    assert!(d.exists(CHIRPSTACK) && d.exists(REGION) && d.exists(FORWARDER));

    // Same /run, same /data, new overlay — exactly what re-provisioning then re-running
    // proximos-render.service looks like.
    std::fs::create_dir_all(d.paths.data.join("proximos/config")).unwrap();
    std::fs::write(
        d.paths.data.join("proximos/config/proximos.config.yaml"),
        "radio:\n  mode: p2p\n",
    )
    .unwrap();

    let report = d.render().expect("second render");

    assert!(!d.exists(CHIRPSTACK));
    assert!(!d.exists(REGION));
    assert!(!d.exists(FORWARDER));
    assert_eq!(report.removed.len(), 3, "{:?}", report.removed);
    // The two unconditional ones survive the flip.
    assert!(d.exists(PORTAL) && d.exists(CONTROL));
}

/// A region file left behind by a render with a different `radio.region` is not inert:
/// ChirpStack is started with `-c <DIR>` and loads every `region_*.toml` it finds there.
#[test]
fn a_region_file_from_another_region_is_removed() {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("first render");

    let stale = d.run_file("chirpstack/region_us915.toml");
    std::fs::write(&stale, "# left over from a different radio.region\n").unwrap();

    let report = d.render().expect("second render");
    assert!(!stale.exists(), "stale region file survived");
    assert!(report.removed.iter().any(|p| p == &stale), "{report:?}");
    assert!(d.exists(REGION), "the current region file was kept");
}

/// The renderer runs on every boot. Twice against unchanged inputs must produce the same
/// bytes — including `db_key`, which is generated once and then reused forever.
#[test]
fn a_second_render_is_byte_identical() {
    let d = Device::new().with_api_secret();
    d.render().expect("first render");
    let first: Vec<String> = d.run_tree().iter().map(|p| d.read_run(p)).collect();
    let key_after_first = std::fs::read_to_string(d.paths.data.join("portal/db.key")).unwrap();

    d.render().expect("second render");
    let second: Vec<String> = d.run_tree().iter().map(|p| d.read_run(p)).collect();
    let key_after_second = std::fs::read_to_string(d.paths.data.join("portal/db.key")).unwrap();

    assert_eq!(first, second);
    assert_eq!(
        key_after_first, key_after_second,
        "db.key was regenerated on the second boot — every existing database row just \
         became unreadable"
    );
}

/// One value, four rendered places. `enabled_regions`, the region file's name, its
/// `[[regions]] name` and the topic prefix on both sides of the gateway backend must all
/// be the same string, or the chain fails silently: both processes stay running, both
/// report healthy, and no uplink is ever seen by the network server.
#[test]
fn region_agrees_across_every_file_that_names_it() {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("render");

    let cs = d.read_run(CHIRPSTACK);
    let region = d.read_run(REGION);
    let fwd = d.read_run(FORWARDER);

    assert!(cs.contains(r#"enabled_regions = ["eu868"]"#), "{cs}");
    assert!(region.contains(r#"name = "eu868""#), "{region}");
    assert!(region.contains(r#"common_name = "EU868""#), "{region}");
    assert!(region.contains(r#"topic_prefix = "eu868""#), "{region}");
    assert!(fwd.contains(r#"topic_prefix="eu868""#), "{fwd}");

    // And the two topic prefixes are the *same* line's worth of value, not two literals
    // that happen to agree today.
    let cs_prefix = region
        .lines()
        .find(|l| l.trim_start().starts_with("topic_prefix"))
        .unwrap();
    let fwd_prefix = fwd
        .lines()
        .find(|l| l.trim_start().starts_with("topic_prefix"))
        .unwrap();
    let value = |l: &str| l.split('"').nth(1).unwrap().to_string();
    assert_eq!(value(cs_prefix), value(fwd_prefix));
}

/// Defect ledger #29, pinned. Upstream's compiled-in default is `0.0.0.0:8080`; omitting
/// the key does not mean the safe thing.
#[test]
fn the_chirpstack_api_binds_loopback_only() {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("render");
    let cs = d.read_run(CHIRPSTACK);
    assert!(cs.contains(r#"bind = "127.0.0.1:8080""#), "{cs}");
    // Over the settings only, not the whole file: the comment above that key quotes
    // upstream's 0.0.0.0 default on purpose, and a test that forbade the string would
    // forbid explaining why the key is there.
    let settings: Vec<&str> = cs
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect();
    assert!(
        settings.iter().all(|l| !l.contains("0.0.0.0")),
        "a setting binds a non-loopback address: {settings:?}"
    );
}

/// The api-secret is injected from `/data`, appears in the rendered file, and appears
/// nowhere else — not in the control config, not in the portal's.
#[test]
fn the_api_secret_is_injected_and_confined_to_one_file() {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("render");
    let secret = "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
    assert!(d
        .read_run(CHIRPSTACK)
        .contains(&format!(r#"secret = "{secret}""#)));
    assert!(!d.read_run(CONTROL).contains(secret));
    assert!(!d.read_run(PORTAL).contains(secret));
    assert!(!d.read_run(FORWARDER).contains(secret));
}

/// A device whose migration has not run yet. Missing state on `/data` must never fail the
/// render: a failed unit means systemd tears down the `RuntimeDirectory=` and deletes the
/// portal config this same run just wrote — which is the bug the shim shipped once.
#[test]
fn a_missing_api_secret_skips_chirpstack_and_keeps_the_rest() {
    let d = Device::new().with_db_key();
    let report = d
        .render()
        .expect("render must not fail on missing /data state");

    assert!(d.exists(PORTAL));
    assert!(d.exists(CONTROL));
    assert!(!d.exists(CHIRPSTACK));
    assert!(!d.exists(REGION));
    // The forwarder is unaffected — it needs no secret.
    assert!(d.exists(FORWARDER));

    let skip = report
        .skipped
        .iter()
        .find(|s| s.path.ends_with("chirpstack.toml"))
        .expect("a named skip");
    assert!(
        skip.reason.contains("005-chirpstack-seed.sh"),
        "{}",
        skip.reason
    );
}

/// An empty api-secret is worse than a missing one: it would sign every ChirpStack token
/// with nothing at all.
#[test]
fn an_empty_api_secret_is_refused_rather_than_used() {
    let d = Device::new()
        .with_db_key()
        .write_data("chirpstack/api-secret", "\n");
    let report = d.render().expect("render");
    assert!(!d.exists(CHIRPSTACK));
    assert!(report.skipped.iter().any(|s| s.reason.contains("empty")));
}

/// Decision #5's client/server gate, ported from the shim. An absent marker means server.
#[test]
fn a_client_role_hub_gets_no_portal_config() {
    let d = Device::new()
        .with_db_key()
        .with_api_secret()
        .write_data("proximos/cluster/role", "client\n");
    let report = d.render().expect("render");

    assert!(!d.exists(PORTAL), "a client-role Hub serves no portal");
    assert!(d.exists(CONTROL), "it still has a radio");
    assert!(report.skipped.iter().any(|s| s.reason.contains("client")));
}

#[test]
fn an_absent_or_server_role_marker_renders_the_portal() {
    for marker in [None, Some("server"), Some("server\n"), Some("")] {
        let mut d = Device::new().with_db_key().with_api_secret();
        if let Some(m) = marker {
            d = d.write_data("proximos/cluster/role", m);
        }
        d.render().expect("render");
        assert!(d.exists(PORTAL), "marker {marker:?}");
    }
}

/// A previously rendered portal config must go when the Hub becomes client-role, for the
/// same reason the ChirpStack pair does.
#[test]
fn becoming_a_client_removes_a_previously_rendered_portal_config() {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("first render");
    assert!(d.exists(PORTAL));

    std::fs::create_dir_all(d.paths.data.join("proximos/cluster")).unwrap();
    std::fs::write(d.paths.data.join("proximos/cluster/role"), "client").unwrap();

    let report = d.render().expect("second render");
    assert!(!d.exists(PORTAL));
    assert!(report.removed.iter().any(|p| p.ends_with("config.yaml")));
}

/// cluster.md §2/§4, the Client's service set as rendered files: no portal, no ChirpStack
/// pair (and with it no redis, gated on the same file), a forwarder retargeted at the
/// Server, and a control config carrying the agent's broker override.
#[test]
fn a_client_role_hub_renders_the_client_service_set() {
    let d = Device::new()
        .with_db_key()
        .with_api_secret()
        .joined_as_client();
    let report = d.render().expect("render");

    assert_eq!(
        d.run_tree(),
        vec![FORWARDER.to_string(), CONTROL.to_string()],
        "a Client renders exactly the forwarder and the control config — \
         no portal, no ChirpStack, no 8883 listener"
    );
    assert!(report.removed.is_empty(), "{:?}", report.removed);
    assert!(report.degraded.is_empty(), "{:?}", report.degraded);
    // Portal, chirpstack.toml and the 8883 listener, each with the role as its reason.
    assert_eq!(report.skipped.len(), 3, "{:?}", report.skipped);
    for s in &report.skipped {
        assert!(s.reason.contains("client"), "{}", s.reason);
    }

    assert_golden("client/forwarder.toml", &d.read_run(FORWARDER));
    assert_golden("client/control-config.yaml", &d.read_run(CONTROL));
}

/// The two halves of the Client's connection — the agent's override and the forwarder —
/// are rendered from one read of `/data/proximos/cluster/`, value by value.
#[test]
fn the_client_forwarder_and_agent_override_name_the_same_server_and_account() {
    let d = Device::new()
        .with_db_key()
        .with_api_secret()
        .joined_as_client();
    d.render().expect("render");

    let fwd: toml::Value = d.read_run(FORWARDER).parse().expect("forwarder TOML");
    let mqtt = &fwd["mqtt"];
    assert_eq!(
        mqtt["server"].as_str(),
        Some("ssl://proximos-1a2b3c4d.local:8883")
    );
    assert_eq!(
        mqtt["username"].as_str(),
        Some(Device::CLIENT_PEER_USERNAME)
    );
    assert_eq!(
        mqtt["password"].as_str(),
        Some(Device::CLIENT_PEER_PASSWORD)
    );
    assert_eq!(
        mqtt["ca_cert"].as_str(),
        Some("/data/proximos/cluster/ca.crt")
    );
    // The radio's topic contract with the Server's ChirpStack does not move.
    assert_eq!(mqtt["topic_prefix"].as_str(), Some("eu868"));

    let ctl: serde_yaml::Value = serde_yaml::from_str(&d.read_run(CONTROL)).unwrap();
    let broker = &ctl["mqtt"]["broker"];
    assert_eq!(broker["host"].as_str(), Some(Device::CLIENT_SERVER_HOST));
    assert_eq!(broker["port"].as_u64(), Some(8883));
    assert_eq!(
        broker["username"].as_str(),
        Some(Device::CLIENT_PEER_USERNAME)
    );
    assert!(broker.get("password").is_none(), "never the secret itself");
    assert_eq!(
        broker["password_file"].as_str(),
        Some("/data/proximos/cluster/peer_password")
    );
    assert_eq!(ctl["mqtt"]["tls"]["enabled"].as_bool(), Some(true));
    assert_eq!(
        ctl["mqtt"]["tls"]["ca_cert_path"].as_str(),
        Some("/data/proximos/cluster/ca.crt")
    );
    // The topic root is still the portal's key, on a Client as on a Server.
    assert_eq!(ctl["mqtt"]["topic_prefix"].as_str(), Some("proximos"));
}

/// Decided 2026-10-02: 8883 only on a Server. Any client marker — a damaged payload
/// included — means no listener: the one thing a Client must never do is open its broker
/// to the network. A marker that does not say `client` is a Server.
#[test]
fn only_a_server_renders_the_8883_listener() {
    for marker in [None, Some("server"), Some(""), Some("clients")] {
        let mut d = Device::new().with_db_key().with_api_secret();
        if let Some(m) = marker {
            d = d.write_data("proximos/cluster/role", m);
        }
        d.render().expect("render");
        assert!(d.exists(MOSQUITTO), "marker {marker:?}");
    }

    let d = Device::new()
        .with_db_key()
        .with_api_secret()
        .joined_as_client();
    let report = d.render().expect("render");
    assert!(!d.exists(MOSQUITTO));
    assert!(report
        .skipped
        .iter()
        .any(|s| s.path.ends_with(MOSQUITTO) && s.reason.contains("loopback-only")));

    let d = Device::new()
        .with_db_key()
        .with_api_secret()
        .joined_as_client()
        .write_data("proximos/cluster/peer_password", "short");
    d.render().expect("render");
    assert!(!d.exists(MOSQUITTO), "a damaged Client is still a Client");
}

/// The listener is on the tmpfs only while the Hub is a Server: a render after the join
/// removes the one the previous boot left, as it does the portal's config.
#[test]
fn becoming_a_client_removes_a_previously_rendered_8883_listener() {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("first render");
    assert!(d.exists(MOSQUITTO));

    let d = d.joined_as_client();
    let report = d.render().expect("second render");
    assert!(!d.exists(MOSQUITTO));
    assert!(report.removed.iter().any(|p| p.ends_with(MOSQUITTO)));
}

/// What mosquitto is handed: the 8883 listener and its security options, nothing global —
/// those belong before `per_listener_settings` scopes them, in the static proximos.conf.
#[test]
fn the_8883_listener_is_tls_only_and_ipv4() {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("render");
    let conf = d.read_run(MOSQUITTO);
    let directives: Vec<&str> = conf
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert_eq!(
        directives,
        vec![
            "listener 8883 0.0.0.0",
            "certfile /data/tls/server.crt",
            "keyfile /data/tls/server.key",
            "allow_anonymous false",
            "password_file /data/mosquitto/passwd",
            "acl_file /data/mosquitto/acl",
        ]
    );
}

/// Decision of 2026-10-02: the control config is 0640 root:proximos, so `proximos-control`
/// can read it — the peer password must not be in it, in any form or position. Only the
/// path of the 0600 root file that holds it, which the agent (root) opens itself. The
/// forwarder TOML, 0640 root:root and read by a root forwarder, is the one rendered file
/// that carries the secret.
#[test]
fn the_rendered_control_config_never_contains_the_peer_password() {
    let d = Device::new()
        .with_db_key()
        .with_api_secret()
        .joined_as_client();
    d.render().expect("render");

    let ctl = d.read_run(CONTROL);
    assert!(
        !ctl.contains(Device::CLIENT_PEER_PASSWORD),
        "the peer password leaked into the control config:\n{ctl}"
    );
    assert!(
        !ctl.lines().any(|l| l.trim_start().starts_with("password:")),
        "no `password:` key at all:\n{ctl}"
    );
    assert!(ctl.contains("password_file: \"/data/proximos/cluster/peer_password\""));

    // And the one file that does carry it is the forwarder's.
    assert!(d.read_run(FORWARDER).contains(Device::CLIENT_PEER_PASSWORD));
}

/// A Server renders none of the override, and its forwarder stays on the loopback broker:
/// the server-role goldens are the proof, this pins the absence by name.
#[test]
fn a_server_role_hub_renders_no_broker_override() {
    for marker in [None, Some("server")] {
        let mut d = Device::new().with_db_key().with_api_secret();
        if let Some(m) = marker {
            d = d.write_data("proximos/cluster/role", m);
        }
        d.render().expect("render");
        let ctl: serde_yaml::Value = serde_yaml::from_str(&d.read_run(CONTROL)).unwrap();
        assert!(ctl["mqtt"].get("broker").is_none(), "marker {marker:?}");
        assert!(ctl["mqtt"].get("tls").is_none(), "marker {marker:?}");
        assert_golden("lorawan/forwarder.toml", &d.read_run(FORWARDER));
        assert_golden("lorawan/control-config.yaml", &d.read_run(CONTROL));
    }
}

/// A Server that becomes a Client loses the ChirpStack pair it had rendered, the way it
/// loses its portal config — `ConditionPathExists=` would otherwise start a ChirpStack
/// (and its redis) nobody configured.
#[test]
fn becoming_a_client_removes_a_previously_rendered_chirpstack_pair() {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("first render");
    assert!(d.exists(CHIRPSTACK) && d.exists(REGION) && d.exists(PORTAL));

    let d = d.joined_as_client();
    let report = d.render().expect("second render");

    assert!(!d.exists(CHIRPSTACK));
    assert!(!d.exists(REGION));
    assert!(!d.exists(PORTAL));
    assert!(!d.exists(MOSQUITTO), "and its network listener");
    assert!(d.exists(FORWARDER), "a LoRaWAN Client keeps its radio");
    assert_eq!(report.removed.len(), 4, "{:?}", report.removed);
    assert!(d.read_run(FORWARDER).contains("ssl://"));
}

/// A P2P Client (phase 2) gets neither the ChirpStack pair nor a forwarder, and the mode
/// is the reason given for both — the role adds nothing to a gate that is already shut.
#[test]
fn a_p2p_client_renders_no_forwarder() {
    let d = Device::new()
        .with_db_key()
        .with_api_secret()
        .with_overlay("radio:\n  mode: p2p\n")
        .joined_as_client();
    d.render().expect("render");
    assert!(!d.exists(FORWARDER));
    assert!(!d.exists(CHIRPSTACK));
    assert!(d.read_run(CONTROL).contains("broker:"));
}

/// `role` is written last, so `client` with an incomplete payload is a damaged `/data`.
/// Never fatal (the failure policy), and never a half configuration: no forwarder, no
/// override — the agent stays on its loopback broker — and the reason in the report.
#[test]
fn an_incomplete_cluster_payload_renders_no_forwarder_and_no_override() {
    let cases: [(&str, Option<&str>, &str); 9] = [
        (
            "proximos/cluster/server_host",
            None,
            "server_host does not exist",
        ),
        ("proximos/cluster/server_host", Some("evil\"host"), "quote"),
        (
            "proximos/cluster/server_host",
            Some("a b"),
            "more than one token",
        ),
        // The Server's 8883 listener is IPv4 only (cluster.md §5).
        (
            "proximos/cluster/server_host",
            Some("fd00::1"),
            "IPv6 literal",
        ),
        (
            "proximos/cluster/server_host",
            Some("[fd00::1]"),
            "IPv6 literal",
        ),
        ("proximos/cluster/server_port", Some("1883"), "plaintext"),
        (
            "proximos/cluster/server_port",
            Some("eighty"),
            "not a TCP port",
        ),
        ("proximos/cluster/peer_username", Some("fiber"), "peer-"),
        (
            "proximos/cluster/peer_password",
            Some("short"),
            "16 characters",
        ),
    ];
    for (rel, value, why) in cases {
        let d = Device::new()
            .with_db_key()
            .with_api_secret()
            .joined_as_client();
        let p = d.paths.data.join(rel);
        match value {
            None => std::fs::remove_file(&p).unwrap(),
            Some(v) => std::fs::write(&p, v).unwrap(),
        }
        let report = d
            .render()
            .unwrap_or_else(|e| panic!("{rel}={value:?}: {e}"));

        assert!(!d.exists(FORWARDER), "{rel}={value:?}");
        assert!(!d.exists(PORTAL), "{rel}={value:?}: still a Client");
        assert!(!d.exists(CHIRPSTACK), "{rel}={value:?}: still a Client");
        let ctl = d.read_run(CONTROL);
        assert!(!ctl.contains("broker:"), "{rel}={value:?}:\n{ctl}");
        assert!(!ctl.contains("tls:"), "{rel}={value:?}:\n{ctl}");

        assert!(
            report
                .skipped
                .iter()
                .any(|s| s.path.ends_with(FORWARDER) && s.reason.contains(why)),
            "{rel}={value:?}: {:?}",
            report.skipped
        );
        assert!(
            report
                .degraded
                .iter()
                .any(|s| s.path.ends_with(CONTROL) && s.reason.contains(why)),
            "{rel}={value:?}: {:?}",
            report.degraded
        );
    }
}

/// The CA is the agent's and the forwarder's to open, not the renderer's: a missing one
/// still renders the override, so the agent can refuse by name (cluster.md §4.5) instead
/// of silently staying on loopback.
#[test]
fn a_missing_ca_still_renders_the_override_for_the_agent_to_refuse() {
    let d = Device::new()
        .with_db_key()
        .with_api_secret()
        .joined_as_client();
    std::fs::remove_file(d.paths.data.join("proximos/cluster/ca.crt")).unwrap();
    let report = d.render().expect("render");
    assert!(report.degraded.is_empty(), "{:?}", report.degraded);
    assert!(d
        .read_run(CONTROL)
        .contains("ca_cert_path: \"/data/proximos/cluster/ca.crt\""));
}

/// Whitespace around a hand-written value is not part of it, and an IPv4 literal goes into
/// the URL and the override as it is.
#[test]
fn cluster_values_are_trimmed_and_an_ipv4_server_is_rendered_as_is() {
    let d = Device::new()
        .with_db_key()
        .with_api_secret()
        .joined_as_client()
        .write_data("proximos/cluster/server_host", " 192.168.1.20\n")
        .write_data("proximos/cluster/server_port", " 8883\n");
    d.render().expect("render");
    let fwd: toml::Value = d.read_run(FORWARDER).parse().unwrap();
    assert_eq!(
        fwd["mqtt"]["server"].as_str(),
        Some("ssl://192.168.1.20:8883")
    );
    let ctl: serde_yaml::Value = serde_yaml::from_str(&d.read_run(CONTROL)).unwrap();
    assert_eq!(ctl["mqtt"]["broker"]["host"].as_str(), Some("192.168.1.20"));
    assert_eq!(ctl["mqtt"]["broker"]["port"].as_u64(), Some(8883));
}

/// The typed merge, end to end: an overlay changes one leaf of one profile and the rest
/// of the tree survives.
#[test]
fn an_overlay_merges_by_key_rather_than_replacing_the_section() {
    let d = Device::new().with_db_key().with_api_secret().with_overlay(
        "radio:\n  device_profiles:\n    otaa:\n      default:\n        uplink_interval: 7200\n",
    );
    d.render().expect("render");
    let control = d.read_run(CONTROL);

    assert!(control.contains("uplink_interval: 7200"));
    // Everything else the overlay did not mention is still there.
    assert!(control.contains("decoder: node"));
    assert!(control.contains("mac_version: LORAWAN_1_0_4"));
    assert!(control.contains("name: ProXimos Node (OTAA)"));
    assert!(control.contains("mode: lorawan"));
}

/// An overlay may add a whole named profile — the arms are maps, and a fleet with two
/// Node variants is the case they exist for.
#[test]
fn an_overlay_can_add_a_named_profile() {
    let d = Device::new().with_db_key().with_api_secret().with_overlay(
        "radio:\n  device_profiles:\n    otaa:\n      slow:\n        name: \"Slow Node\"\n        \
         mac_version: LORAWAN_1_0_4\n        reg_params_revision: RP002_1_0_4\n        \
         uplink_interval: 86400\n        device_status_req_interval: 1\n        decoder: node\n",
    );
    d.render().expect("render");
    let control = d.read_run(CONTROL);
    assert!(control.contains("slow:"));
    assert!(control.contains("default:"));
}

/// The legacy portal-only override the shim appended verbatim. Still honoured, now merged
/// by key, so the emitted document carries each key exactly once instead of relying on a
/// YAML parser's last-key-wins behaviour.
#[test]
fn the_legacy_portal_override_is_merged_not_appended() {
    let d = Device::new().with_db_key().with_api_secret().write_data(
        "portal/config.yaml",
        "cookie_secure: false\nmqtt_port: 1884\n",
    );
    d.render().expect("render");
    let portal = d.read_run(PORTAL);

    assert_eq!(portal.matches("cookie_secure:").count(), 1, "{portal}");
    assert_eq!(portal.matches("mqtt_port:").count(), 1, "{portal}");
    assert!(portal.contains("cookie_secure: false"));
    assert!(portal.contains("mqtt_port: 1884"));
    // And the keys it did not mention are untouched.
    assert!(portal.contains(r#"bind_addr: "127.0.0.1:8088""#));
}

/// The three optional portal keys are emitted only when something sets them, so an
/// unconfigured device gets exactly the shim's nine lines.
#[test]
fn the_optional_portal_keys_appear_only_when_set() {
    let bare = Device::new().with_db_key().with_api_secret();
    bare.render().expect("render");
    let text = bare.read_run(PORTAL);
    assert!(!text.contains("mqtt_username"));
    assert!(!text.contains("experimental_features_enabled"));

    let configured = Device::new().with_db_key().with_api_secret().write_data(
        "portal/config.yaml",
        "mqtt_username: hub\nmqtt_password: secret\nexperimental_features_enabled: true\n",
    );
    configured.render().expect("render");
    assert_golden(
        "overrides/portal-config-with-optionals.yaml",
        &configured.read_run(PORTAL),
    );
}

/// WP0.3's three device-tuning sections, and the calibration reminder's, travel the same
/// optional path as the three scalar keys above: absent by default (so shim parity holds),
/// and rendered as a YAML flow mapping the portal's own `deny_unknown_fields` sections
/// parse back.
#[test]
fn the_device_tuning_sections_render_only_when_an_overlay_sets_them() {
    let bare = Device::new().with_db_key().with_api_secret();
    bare.render().expect("render");
    let text = bare.read_run(PORTAL);
    assert!(!text.contains("nodes:"), "{text}");
    assert!(!text.contains("beacons:"), "{text}");
    assert!(!text.contains("signal_quality:"), "{text}");
    assert!(!text.contains("calibration:"), "{text}");

    let configured = Device::new().with_db_key().with_api_secret().write_data(
        "portal/config.yaml",
        "nodes:\n  offline_detector: 6\nbeacons:\n  store_interval: 30\n  offline_detector: 2\n\
         calibration:\n  warning_days: 45\n  second_warning_days: 10\n",
    );
    configured.render().expect("render");
    let text = configured.read_run(PORTAL);

    // Asserted by parsing, not by substring: the point is that the portal can read it
    // back, and the flow-mapping spelling is an implementation detail of the template.
    let parsed: serde_yaml::Value = serde_yaml::from_str(&text).expect("rendered file parses");
    assert_eq!(parsed["nodes"]["offline_detector"].as_u64(), Some(6));
    assert_eq!(parsed["beacons"]["store_interval"].as_u64(), Some(30));
    assert_eq!(parsed["beacons"]["offline_detector"].as_u64(), Some(2));
    assert_eq!(parsed["calibration"]["warning_days"].as_u64(), Some(45));
    assert_eq!(
        parsed["calibration"]["second_warning_days"].as_u64(),
        Some(10)
    );
    // A section nobody set is still absent, not rendered empty.
    assert!(parsed.get("signal_quality").is_none(), "{text}");
    // And the nine shim keys are untouched around it.
    assert_eq!(parsed["bind_addr"].as_str(), Some("127.0.0.1:8088"));
}

/// plan/portal/threshold_defaults.md §2: every Hub renders the default bands from
/// `defaults.yaml`, and a site overlay moves one bound without restating the rest (the
/// typed merge is key by key), or drops one with `null`.
#[test]
fn the_default_bands_render_everywhere_and_an_overlay_moves_one_bound() {
    let bare = Device::new().with_db_key().with_api_secret();
    bare.render().expect("render");
    let parsed: serde_yaml::Value =
        serde_yaml::from_str(&bare.read_run(PORTAL)).expect("rendered file parses");
    let t = &parsed["thresholds"]["defaults"]["temperature"];
    assert_eq!(t["critical_low"].as_i64(), Some(-10));
    assert_eq!(t["critical_high"].as_i64(), Some(50));
    assert_eq!(
        parsed["thresholds"]["defaults"]["humidity"]["warning_high"].as_i64(),
        Some(80)
    );

    let site = Device::new().with_db_key().with_api_secret().with_overlay(
        "portal:\n  thresholds:\n    defaults:\n      temperature:\n        critical_low: -20\n        \
         warning_high: null\n",
    );
    site.render().expect("render");
    let parsed: serde_yaml::Value =
        serde_yaml::from_str(&site.read_run(PORTAL)).expect("rendered file parses");
    let t = &parsed["thresholds"]["defaults"]["temperature"];
    assert_eq!(t["critical_low"].as_i64(), Some(-20));
    assert_eq!(
        t["warning_low"].as_i64(),
        Some(0),
        "kept from defaults.yaml"
    );
    assert!(t["warning_high"].is_null(), "dropped by the overlay");
    assert_eq!(
        parsed["thresholds"]["defaults"]["humidity"]["critical_high"].as_i64(),
        Some(90)
    );
}

/// A misspelled override key is a named failure, not a setting that silently does
/// nothing. This is the tightening over the shim's concatenation, so it is pinned.
#[test]
fn an_unknown_override_key_fails_the_render_and_names_itself() {
    let d = Device::new()
        .with_db_key()
        .write_data("portal/config.yaml", "bind_adr: \"0.0.0.0:80\"\n");
    let err = d.render().expect_err("an unknown key must not be ignored");
    let msg = err.to_string();
    assert!(msg.contains("bind_adr"), "{msg}");
    assert!(msg.contains("portal"), "{msg}");
}

/// `db_key` has one authority: `/data/portal/db.key`. A passphrase in a config file is a
/// passphrase in the next backup and support bundle.
#[test]
fn db_key_cannot_be_set_from_an_override_file() {
    let d = Device::new()
        .with_db_key()
        .write_data("portal/config.yaml", "db_key: \"pasted-by-hand\"\n");
    let err = d.render().expect_err("db_key must not be settable");
    assert!(err.to_string().contains("db_key"), "{err}");
}

/// An unknown `radio.mode` is fatal, before anything is written. There is no sensible
/// partial output for "the configuration is wrong", and guessing a mode would pick which
/// services a medical device runs.
#[test]
fn an_unknown_radio_mode_fails_the_render_before_writing_anything() {
    let d = Device::new()
        .with_db_key()
        .with_api_secret()
        .with_overlay("radio:\n  mode: sometimes\n");
    let err = d.render().expect_err("an unknown mode must be fatal");
    let msg = err.to_string();
    // The message quotes the offending value and lists what was expected, so an operator
    // reading `journalctl -u proximos-render` can fix it without opening the source.
    assert!(msg.contains("sometimes"), "{msg}");
    assert!(msg.contains("off") && msg.contains("lorawan"), "{msg}");
    assert!(msg.contains("radio"), "{msg}");
    assert!(
        d.run_tree().is_empty(),
        "nothing may be written when the configuration does not parse: {:?}",
        d.run_tree()
    );
}

/// Atomicity, from the outside: a failed render leaves no `.tmp.<pid>` file anywhere, and
/// in particular no 0640 file holding a database passphrase under a name nothing cleans
/// up.
#[test]
fn a_failed_render_leaves_no_temp_debris() {
    let d = Device::new()
        .with_db_key()
        .with_api_secret()
        // The region template for a region nobody shipped: this fails *after* the portal
        // and control configs are already on disk, which is the interesting case.
        .with_overlay("radio:\n  region: eu868\n");
    // Force a template failure by pointing the templates root at a tree missing the
    // region file, without touching the shipped one.
    let broken = d.dir.path().join("templates");
    copy_tree(&common::templates_dir(), &broken);
    std::fs::remove_file(broken.join("chirpstack/region_eu868.toml.j2")).unwrap();
    let paths = proximos_render::RenderPaths {
        templates: broken,
        ..d.paths.clone()
    };

    let err = proximos_render::render_all(&paths, d.default_mode)
        .expect_err("a missing template is fatal");
    assert!(err.to_string().contains("region_eu868.toml.j2"), "{err}");

    let tree = d.run_tree();
    assert!(
        tree.iter().all(|p| !p.contains(".tmp.")),
        "temp debris survived: {tree:?}"
    );
    // The half of the render that had already succeeded is still there, intact.
    assert!(d.exists("portal/config.yaml"));
}

/// `PROXIMOS_CONFIG` names the file control-radio reads; the renderer writes the same one.
#[test]
fn proximos_config_redirects_the_control_config() {
    let d = Device::new().with_db_key().with_api_secret();
    let elsewhere = d.dir.path().join("elsewhere/config.yaml");
    let paths = proximos_render::RenderPaths {
        control_config: Some(elsewhere.clone()),
        ..d.paths.clone()
    };
    proximos_render::render_all(&paths, d.default_mode).expect("render");

    assert!(elsewhere.exists());
    assert!(!d.exists("control/config.yaml"));
    assert!(std::fs::read_to_string(&elsewhere)
        .unwrap()
        .contains("radio:"));
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let dst = to.join(e.file_name());
        if e.path().is_dir() {
            copy_tree(&e.path(), &dst);
        } else {
            std::fs::copy(e.path(), dst).unwrap();
        }
    }
}

/// node_registry.md §11 (amended 2026-09-27 R7): the Hub's runtime `radio.mode` lives in
/// `fiber.service`'s own config and wins over the defaults and the device overlay, so a
/// re-render after `radio-set-mode` renders the new mode — the ChirpStack pair included.
#[test]
fn the_hubs_runtime_radio_mode_wins_over_the_defaults_and_the_overlay() {
    let d = Device::new()
        .with_db_key()
        .with_api_secret()
        .with_overlay("radio:\n  mode: lorawan\n")
        .write_data(
            HUB_CONFIG,
            "power:\n  poll_ms: 1000\nradio:\n  mode: p2p\nsystem:\n  system_info_sec: 60\n",
        );
    d.render().expect("render");
    assert!(
        d.read_run(CONTROL).contains("mode: p2p"),
        "{}",
        d.read_run(CONTROL)
    );
    assert!(
        !d.exists(CHIRPSTACK) && !d.exists(FORWARDER),
        "p2p runs no ChirpStack"
    );

    // Back to LoRaWAN at runtime: the pair comes back on the same /run.
    std::fs::write(d.paths.data.join(HUB_CONFIG), "radio:\n  mode: dual\n").unwrap();
    d.render().expect("re-render");
    assert!(d.read_run(CONTROL).contains("mode: dual"));
    assert!(d.exists(CHIRPSTACK) && d.exists(FORWARDER));
}

/// No `radio:` in the Hub's config, or no file at all, is "no runtime setting": the hardware
/// default applies. A mode that is not one of the four is fatal, like one in the overlay.
#[test]
fn the_hubs_config_without_a_mode_leaves_the_default_and_a_bad_mode_is_fatal() {
    for hub_config in [
        None,
        Some("system:\n  system_info_sec: 60\n"),
        Some("radio:\n"),
    ] {
        let d = Device::new().with_db_key().with_api_secret();
        let d = match hub_config {
            Some(text) => d.write_data(HUB_CONFIG, text),
            None => d,
        };
        d.render().expect("render");
        assert!(
            d.read_run(CONTROL).contains("mode: lorawan"),
            "{hub_config:?}: the default of a Hub with the card"
        );
    }

    let d = Device::new()
        .with_db_key()
        .write_data(HUB_CONFIG, "radio:\n  mode: both\n");
    let err = d.render().expect_err("an unknown mode is fatal");
    assert!(err.to_string().contains("both"), "{err}");
    assert!(err.to_string().contains("fiber.config.yaml"), "{err}");
    assert!(!d.exists(CONTROL), "nothing was written");
}

// ---------------------------------------------------------------------------------------
// node_registry.md §12: the default mode follows the hardware
// ---------------------------------------------------------------------------------------

const HUB_CONFIG: &str = "fiber/config/fiber.config.yaml";

/// `defaults.yaml` names no mode any more: a mode in it would win over the hardware default
/// for every Hub, which is exactly the phase-1 `lorawan` the default replaces.
#[test]
fn the_shipped_defaults_name_no_radio_mode() {
    let text = std::fs::read_to_string(common::templates_dir().join("defaults.yaml")).unwrap();
    let doc: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
    assert!(
        doc["radio"].is_mapping(),
        "the radio section is still there"
    );
    assert!(
        doc["radio"].get("mode").is_none(),
        "defaults.yaml names a mode"
    );
}

/// A Hub with the RAK5146 and nothing chosen runs LoRaWAN: the ChirpStack chain is rendered.
#[test]
fn with_the_card_and_nothing_chosen_the_hub_runs_lorawan() {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("render");
    assert!(d.read_run(CONTROL).contains("mode: lorawan"));
    assert!(d.exists(CHIRPSTACK) && d.exists(FORWARDER));
    // The key keeps the place it had in defaults.yaml, so the golden does not move.
    assert_golden("lorawan/control-config.yaml", &d.read_run(CONTROL));
}

/// Without the card, P2P: no ChirpStack configs, and none left behind by an earlier render.
#[test]
fn without_the_card_and_nothing_chosen_the_hub_runs_p2p() {
    let d = Device::new().with_db_key().with_api_secret().without_card();
    let report = d.render().expect("render");
    let control = d.read_run(CONTROL);
    assert!(control.contains("mode: p2p"), "{control}");
    assert!(!d.exists(CHIRPSTACK) && !d.exists(FORWARDER));
    assert!(
        report.skipped.iter().all(|s| s.reason.contains("p2p")),
        "{:?}",
        report.skipped
    );
    // Same place in the section as the golden's `lorawan`: right after `region`.
    let radio: Vec<&str> = control
        .lines()
        .skip_while(|l| *l != "radio:")
        .take(3)
        .collect();
    assert_eq!(radio, ["radio:", "  region: eu868", "  mode: p2p"]);
}

/// A chosen mode (provisioning or `radio-set-mode`, in `fiber.service`'s config) wins over the
/// hardware default either way round — the renderer does not second-guess it. That the choice
/// suits the hardware is the check the writers make.
#[test]
fn a_chosen_mode_wins_over_the_hardware_default() {
    let d = Device::new()
        .with_db_key()
        .with_api_secret()
        .write_data(HUB_CONFIG, "radio:\n  mode: p2p\n");
    d.render().expect("render");
    assert!(d.read_run(CONTROL).contains("mode: p2p"));
    assert!(!d.exists(CHIRPSTACK));

    // A card that did not enumerate this boot: the choice made at provisioning still holds.
    let d = Device::new()
        .with_db_key()
        .with_api_secret()
        .without_card()
        .write_data(HUB_CONFIG, "radio:\n  mode: lorawan\n");
    d.render().expect("render");
    assert!(d.read_run(CONTROL).contains("mode: lorawan"));
    assert!(d.exists(CHIRPSTACK));
}

/// A mode the device overlay names wins over the hardware default too; the Hub's own choice
/// still wins over the overlay (`the_hubs_runtime_radio_mode_wins_over_the_defaults_and_the_overlay`).
#[test]
fn an_overlay_mode_wins_over_the_hardware_default() {
    for (card, overlay, want) in [
        (true, "p2p", "p2p"),
        (false, "lorawan", "lorawan"),
        (false, "off", "off"),
    ] {
        let d = Device::new()
            .with_db_key()
            .with_api_secret()
            .with_overlay(&format!("radio:\n  mode: {overlay}\n"));
        let d = if card { d } else { d.without_card() };
        d.render().expect("render");
        assert!(
            d.read_run(CONTROL).contains(&format!("mode: {want}")),
            "card {card}, overlay {overlay}"
        );
    }

    // An overlay that says `mode:` with no value names nothing: the default applies.
    let d = Device::new()
        .with_db_key()
        .without_card()
        .with_overlay("radio:\n  mode:\n");
    d.render().expect("render");
    assert!(d.read_run(CONTROL).contains("mode: p2p"));
}
