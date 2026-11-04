//! The two TOML outputs, parsed back.
//!
//! Necessary because they are produced by templating text rather than by a serializer — a
//! serializer cannot carry the comments that are half the point of these files, and every
//! one of those comments records something that was learned the expensive way. The price
//! is that "still valid TOML" stops being a type-system guarantee, and the thing it guards
//! is a network server that refuses to start on a Hub in the field.
//!
//! Structure, not just syntax: `[regions.gateway.backend.mqtt]` nested under a
//! `[[regions]]` array-of-tables is the shape ChirpStack actually reads, and getting the
//! table headers subtly wrong produces a file that parses fine and configures nothing.

mod common;

use common::Device;

fn rendered() -> (toml::Value, toml::Value, toml::Value) {
    let d = Device::new().with_db_key().with_api_secret();
    d.render().expect("render");
    let p = |rel: &str| {
        d.read_run(rel)
            .parse::<toml::Value>()
            .unwrap_or_else(|e| panic!("{rel} is not valid TOML: {e}"))
    };
    (
        p("chirpstack/chirpstack.toml"),
        p("chirpstack/region_eu868.toml"),
        p("chirpstack-mqtt-forwarder/chirpstack-mqtt-forwarder.toml"),
    )
}

#[test]
fn chirpstack_toml_has_the_sections_the_sqlite_build_reads() {
    let (cs, _, _) = rendered();

    assert_eq!(cs["api"]["bind"].as_str(), Some("127.0.0.1:8080"));
    assert!(cs["api"]["secret"].as_str().is_some_and(|s| !s.is_empty()));

    // The SQLite artifact's section. A [postgresql] section here would mean the wrong
    // upstream build was pinned — they are two different binaries of the same version.
    assert_eq!(
        cs["sqlite"]["path"].as_str(),
        Some("sqlite:///data/chirpstack/chirpstack.sqlite")
    );
    assert!(cs.get("postgresql").is_none());

    assert_eq!(
        cs["redis"]["servers"].as_array().map(Vec::as_slice),
        Some([toml::Value::from("redis://127.0.0.1:6379")].as_slice())
    );
    assert_eq!(
        cs["network"]["enabled_regions"]
            .as_array()
            .map(Vec::as_slice),
        Some([toml::Value::from("eu868")].as_slice())
    );
    assert_eq!(cs["network"]["net_id"].as_str(), Some("000000"));

    assert_eq!(
        cs["integration"]["enabled"].as_array().map(Vec::as_slice),
        Some([toml::Value::from("mqtt")].as_slice())
    );
    // The marshaler. WP-D's event decoder deserializes pbjson; protobuf on the same topics
    // would make every event fail to parse.
    assert_eq!(cs["integration"]["mqtt"]["json"].as_bool(), Some(true));
    assert_eq!(
        cs["integration"]["mqtt"]["server"].as_str(),
        Some("tcp://127.0.0.1:1883")
    );
}

#[test]
fn the_region_file_is_an_array_of_tables_with_the_gateway_backend_nested_correctly() {
    let (_, region, _) = rendered();

    let regions = region["regions"].as_array().expect("[[regions]]");
    assert_eq!(regions.len(), 1);
    let r = &regions[0];
    assert_eq!(r["name"].as_str(), Some("eu868"));
    assert_eq!(r["common_name"].as_str(), Some("EU868"));
    assert_eq!(r["gateway"]["force_gws_private"].as_bool(), Some(false));
    assert_eq!(r["gateway"]["backend"]["enabled"].as_str(), Some("mqtt"));
    assert_eq!(
        r["gateway"]["backend"]["mqtt"]["topic_prefix"].as_str(),
        Some("eu868")
    );
    assert_eq!(
        r["gateway"]["backend"]["mqtt"]["server"].as_str(),
        Some("tcp://127.0.0.1:1883")
    );
    assert_eq!(r["network"]["rx2_frequency"].as_integer(), Some(869525000));
}

/// #95: `min_dr`/`max_dr` under `[regions.network]` are ChirpStack's ADR bounds, and its
/// `RegionNetwork` defaults both to 0 when they are missing. The per-channel pair under
/// `extra_channels` only says which rates a channel carries; without the network-level
/// pair every ADR decision is clamped to DR0/SF12.
#[test]
fn the_region_file_lets_adr_use_every_eu868_lora_rate() {
    let (_, region, _) = rendered();
    let network = &region["regions"].as_array().unwrap()[0]["network"];
    assert_eq!(network["min_dr"].as_integer(), Some(0));
    assert_eq!(network["max_dr"].as_integer(), Some(5));
}

#[test]
fn the_forwarder_toml_keeps_the_shipped_files_shape() {
    let (_, _, fwd) = rendered();

    assert_eq!(fwd["backend"]["enabled"].as_str(), Some("concentratord"));
    assert_eq!(
        fwd["backend"]["concentratord"]["event_url"].as_str(),
        Some("ipc:///run/chirpstack-concentratord/event")
    );
    assert_eq!(
        fwd["backend"]["concentratord"]["command_url"].as_str(),
        Some("ipc:///run/chirpstack-concentratord/command")
    );
    assert_eq!(fwd["mqtt"]["topic_prefix"].as_str(), Some("eu868"));
    assert_eq!(fwd["mqtt"]["server"].as_str(), Some("tcp://127.0.0.1:1883"));

    // The credential and TLS fields stay present and empty, exactly as the static /etc
    // file shipped them: filling them in before mosquitto grows a password file would
    // simply stop the forwarder connecting.
    for k in ["username", "password", "ca_cert", "tls_cert", "tls_key"] {
        assert_eq!(fwd["mqtt"][k].as_str(), Some(""), "{k}");
    }
    assert_eq!(fwd["logging"]["log_to_syslog"].as_bool(), Some(false));
}

/// The cross-file contract, asserted on the parsed values rather than on two grep hits:
/// ChirpStack subscribes under its region file's prefix and the forwarder publishes under
/// its own. A mismatch leaves both processes running, both reporting healthy, and no
/// uplink ever reaching the network server.
#[test]
fn both_sides_of_the_gateway_backend_carry_the_same_topic_prefix() {
    let (cs, region, fwd) = rendered();
    let a = region["regions"].as_array().unwrap()[0]["gateway"]["backend"]["mqtt"]["topic_prefix"]
        .as_str()
        .unwrap();
    let b = fwd["mqtt"]["topic_prefix"].as_str().unwrap();
    assert_eq!(a, b);
    // ...and the region that names them is the one chirpstack.toml enables.
    assert_eq!(
        cs["network"]["enabled_regions"].as_array().unwrap()[0]
            .as_str()
            .unwrap(),
        a
    );
}
