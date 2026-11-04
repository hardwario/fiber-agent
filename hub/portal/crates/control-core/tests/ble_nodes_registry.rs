//! FB0D/FB11/FB12 on a radio-mode Hub go through the Node registry — GitLab #118,
//! `plan/control/ble/peripheral.md` §4 (amended 2026-10-10).
//!
//! The three BLE Node verbs used to take the legacy `libs::lorawan` path on every Hub. On a Hub
//! whose `radio.mode` names a transport that path is gated off, so the phone could not add a
//! Node, did not see the ones the Portal added, and could not remove them. They now go where the
//! Portal's signed `node-add`/`node-remove` go, and FB11 reads the registry those write.
//!
//! What runs here is the shipped code from `NodeEnroll` down: the routing rule, the mapping of an
//! FB0D request onto the Portal's `Op`, the registry reads, and — for P2P — the shipped arm itself
//! (`radio_nodes::run_p2p`, the CLI's own `node-add --radio p2p`) writing a real
//! `node_registry.json` in a temporary directory. The LoRaWAN arm needs a ChirpStack, so for it
//! the test records the `Op` it was handed and writes the registry record that arm writes; that
//! the arm applies `radio.join_eui` to an `Op` without one is control-radio's F22 test.
//!
//! No process-global state is touched (`PROXIMOS_CONFIG` stays unset): the radio stack is
//! injected through `LorawanNodeEnroll::with_radio`, so every test owns its Hub.

use std::sync::{Arc, Mutex};

use control_ble::host::{NodeAddRequest, NodeEnroll, NodeEntry, NodeRemoveTarget};
use control_radio::config::RadioConfig;
use control_radio::lorawan::fake::FakePublisher;
use control_radio::registry::{self, CarrierRecord, LorawanParams, Registry};
use control_radio::store::Store;
use control_radio::Transport;
use fiber_app::libs::ble_host::node_enroll::{LorawanNodeEnroll, RadioStack};
use fiber_app::libs::lorawan::NodeAddDeps;
use fiber_app::libs::radio_nodes::{self, Op};

const RADIO_SECTION: &str = include_str!("../../control-radio/tests/fixtures/radio-section.yaml");
const APP_KEY: &str = "00112233445566778899AABBCCDDEEFF";

/// What a radio command was asked to do, the AppKey left out.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ran {
    Add {
        radio: String,
        profile: String,
        serial: String,
        dev_eui: String,
        name: Option<String>,
        join_eui: Option<String>,
    },
    Remove {
        radio: String,
        dev_eui: String,
    },
}

/// A Hub's v2 radio stack in a temporary directory.
struct Radio {
    config: RadioConfig,
    store: Store,
    broker: FakePublisher,
    ran: Mutex<Vec<Ran>>,
    dir: tempfile::TempDir,
}

impl Radio {
    fn new(mode: &str) -> Arc<Radio> {
        let yaml = RADIO_SECTION.replacen("mode: lorawan", &format!("mode: {mode}"), 1);
        let dir = tempfile::tempdir().expect("tempdir");
        Arc::new(Radio {
            config: RadioConfig::from_device_config_yaml(&yaml).expect("radio config"),
            store: Store::new(dir.path()),
            broker: FakePublisher::new(),
            ran: Mutex::new(Vec::new()),
            dir,
        })
    }

    fn ran(&self) -> Vec<Ran> {
        self.ran.lock().unwrap().clone()
    }

    fn registry(&self) -> Registry {
        Registry::load(&self.store)
            .expect("readable")
            .unwrap_or_default()
    }

    /// A Node the Portal registered on LoRaWAN: the record `node-add --radio lorawan` writes.
    fn seed_lorawan(&self, dev_eui: &str, serial: &str, name: Option<&str>) {
        registry::apply(
            &self.store,
            Transport::Lorawan,
            &[CarrierRecord {
                dev_eui: dev_eui.to_string(),
                serial: serial.to_string(),
                profile: "default".into(),
                name: name.map(str::to_string),
                app_key: Some(APP_KEY.to_lowercase()),
                lorawan: Some(LorawanParams {
                    activation: "otaa".into(),
                    join_eui: None,
                }),
            }],
            &[],
        )
        .expect("seeded");
    }

    /// A Node the Portal registered on P2P: its signed `node-add`, through the shipped arm.
    fn seed_p2p(&self, dev_eui: &str, serial: &str, name: Option<&str>) {
        radio_nodes::run_p2p(
            &self.config,
            Op::Add {
                radio: "p2p".into(),
                profile: "default".into(),
                serial: serial.into(),
                dev_eui: dev_eui.into(),
                name: name.map(str::to_string),
                app_key: APP_KEY.to_lowercase(),
                join_eui: None,
            },
            self.store.clone(),
            Some(&self.broker),
        )
        .expect("registered");
    }

    /// The retained `info` payloads published for `dev_eui`, oldest first.
    fn info(&self, dev_eui: &str) -> Vec<Vec<u8>> {
        self.broker
            .published()
            .into_iter()
            .filter(|(t, _)| t.ends_with(&format!("/nodes/{dev_eui}/info")))
            .map(|(_, p)| p)
            .collect()
    }
}

impl RadioStack for Radio {
    fn config(&self) -> control_radio::Result<RadioConfig> {
        Ok(self.config.clone())
    }

    fn registry(&self) -> control_radio::Result<Option<Registry>> {
        Registry::load(&self.store)
    }

    fn run(&self, op: Op) -> Result<(), String> {
        let ran = match &op {
            Op::Add {
                radio,
                profile,
                serial,
                dev_eui,
                name,
                join_eui,
                ..
            } => Ran::Add {
                radio: radio.clone(),
                profile: profile.clone(),
                serial: serial.clone(),
                dev_eui: dev_eui.clone(),
                name: name.clone(),
                join_eui: join_eui.clone(),
            },
            Op::Remove { radio, dev_eui } => Ran::Remove {
                radio: radio.clone(),
                dev_eui: dev_eui.clone(),
            },
            Op::Move { .. } => panic!("the BLE verbs never move a Node"),
        };
        self.ran.lock().unwrap().push(ran.clone());
        match ran {
            Ran::Add { radio, .. } | Ran::Remove { radio, .. } if radio == "p2p" => {
                radio_nodes::run_p2p(&self.config, op, self.store.clone(), Some(&self.broker))
            }
            // The LoRaWAN arm's registry write, standing in for the ChirpStack round trip.
            Ran::Add {
                serial,
                dev_eui,
                name,
                join_eui,
                ..
            } => registry::apply(
                &self.store,
                Transport::Lorawan,
                &[CarrierRecord {
                    dev_eui,
                    serial,
                    profile: "default".into(),
                    name,
                    app_key: Some(APP_KEY.to_lowercase()),
                    lorawan: Some(LorawanParams {
                        activation: "otaa".into(),
                        join_eui: join_eui.or_else(|| self.config.join_eui.clone()),
                    }),
                }],
                &[],
            )
            .map_err(|e| e.to_string()),
            Ran::Remove { dev_eui, .. } => {
                registry::apply(&self.store, Transport::Lorawan, &[], &[dev_eui])
                    .map_err(|e| e.to_string())
            }
        }
    }
}

/// The Hub's BLE Node seam over `radio`, with no legacy dependencies wired: on a radio-mode Hub
/// nothing may reach them.
fn enroll(radio: &Arc<Radio>) -> LorawanNodeEnroll {
    LorawanNodeEnroll::new(
        NodeAddDeps::default(),
        radio.dir.path().join("fiber.config.yaml"),
    )
    .with_radio(radio.clone())
}

/// An FB0D write as the Manager app sends it for a Node whose tag carries no JoinEUI.
fn fb0d(deveui: &str, serial: &str, name: &str) -> NodeAddRequest {
    NodeAddRequest {
        deveui: deveui.into(),
        profile_id: Some(1),
        joineui: "0000000000000000".into(),
        appkey: APP_KEY.into(),
        name: name.into(),
        serial_number: serial.into(),
    }
}

fn entry(deveui: &str, serial: &str, name: &str) -> NodeEntry {
    NodeEntry {
        deveui: deveui.into(),
        serial: Some(serial.into()),
        name: Some(name.into()),
    }
}

fn listed(e: &LorawanNodeEnroll) -> Vec<NodeEntry> {
    let mut list = e.list().expect("FB11 answers");
    list.sort_by(|a, b| a.deveui.cmp(&b.deveui));
    list
}

/// The issue's acceptance test: an add through FB0D's `NodeEnroll::add` is listed by FB11's
/// `NodeEnroll::list`, against the registry — on a P2P Hub, through the shipped P2P arm.
#[test]
fn fb0d_on_a_p2p_hub_registers_the_node_and_fb11_lists_it() {
    let radio = Radio::new("p2p");
    let e = enroll(&radio);

    let r = e.add(&fb0d("5876070000000902", "2162190902", "Cold room"));
    assert!(r.success, "{r:?}");
    assert!(!r.pending, "the answer is final");
    assert_eq!(r.deveui, "5876070000000902");

    // The Portal's registration: P2P on a P2P Hub, the default profile, the name, and no JoinEUI.
    assert_eq!(
        radio.ran(),
        vec![Ran::Add {
            radio: "p2p".into(),
            profile: "default".into(),
            serial: "2162190902".into(),
            dev_eui: "5876070000000902".into(),
            name: Some("Cold room".into()),
            join_eui: None,
        }]
    );
    let node = radio
        .registry()
        .get("5876070000000902")
        .cloned()
        .expect("in node_registry.json");
    assert_eq!(node.radio, Transport::P2p);
    assert_eq!(node.serial, "80e06a36", "canonical, as the Portal's");
    assert_eq!(node.name.as_deref(), Some("Cold room"));
    assert!(
        !radio.info("5876070000000902").is_empty(),
        "the retained info went out, as for a Portal registration"
    );

    // What the app re-reads straight after a successful FB0D.
    assert_eq!(
        listed(&e),
        vec![entry("5876070000000902", "80e06a36", "Cold room")]
    );
}

/// A Node added in the Portal is on the app's list, on either carrier — a LoRaWAN Node on a P2P
/// Hub included: the Hub still holds it (node_registry.md §4).
#[test]
fn fb11_lists_portal_added_nodes_on_both_carriers() {
    let radio = Radio::new("p2p");
    radio.seed_p2p("5876070000005722", "2162165722", Some("Freezer"));
    radio.seed_lorawan("5876070000000413", "2162164755", Some("Cold store"));
    // Before the registry carried names a Node could have none; FB11 omits it then.
    radio.seed_lorawan("5876070000000414", "80e00414", None);

    let e = enroll(&radio);
    assert_eq!(
        listed(&e),
        vec![
            entry("5876070000000413", "80e00413", "Cold store"),
            NodeEntry {
                deveui: "5876070000000414".into(),
                serial: Some("80e00414".into()),
                name: None,
            },
            entry("5876070000005722", "80e007da", "Freezer"),
        ]
    );
    assert!(radio.ran().is_empty(), "a list runs nothing");
}

/// Never written is an empty Hub, not a failure: `proximos-control` writes the registry at its
/// first start, and FB11 must not write it in its place.
#[test]
fn a_registry_never_written_is_an_empty_list_and_stays_unwritten() {
    let radio = Radio::new("lorawan");
    let e = enroll(&radio);
    assert_eq!(e.list().expect("answers"), vec![]);
    assert!(
        !radio.dir.path().join("node_registry.json").exists(),
        "FB11 never migrates the registry"
    );
}

/// An unreadable `node_registry.json` is an ATT-level error on FB11 (`peripheral.md` §4), never
/// `total: 0`, and FB0D/FB12 refuse with the reason before anything runs.
#[test]
fn an_unreadable_registry_is_an_error_and_nothing_runs() {
    let radio = Radio::new("lorawan");
    std::fs::write(radio.dir.path().join("node_registry.json"), "{ not json").expect("write");
    let e = enroll(&radio);

    let err = e.list().expect_err("FB11 cannot answer");
    assert!(err.contains("node_registry.json"), "{err}");

    let r = e.add(&fb0d("5876070000000902", "2162190902", "Cold room"));
    assert!(!r.success && r.message.contains("Node registry"), "{r:?}");
    let out = e.remove(&NodeRemoveTarget::DevEui("5876070000000902".into()));
    assert!(
        !out.success && out.message.contains("Node registry"),
        "{out:?}"
    );
    assert!(radio.ran().is_empty());
}

/// FB12 resolves against the registry and sends the Portal's `node-remove` on the carrier the
/// registry holds the Node on; the Node is then gone from FB11.
#[test]
fn fb12_removes_a_registry_node_by_deveui_and_by_serial() {
    let radio = Radio::new("p2p");
    radio.seed_p2p("5876070000000902", "2162190902", Some("Cold room"));
    radio.seed_p2p("5876070000005722", "2162165722", Some("Freezer"));
    let e = enroll(&radio);

    // By DevEUI, in the case a label prints it.
    let out = e.remove(&NodeRemoveTarget::DevEui("5876070000000902".to_uppercase()));
    assert!(out.success, "{out:?}");
    // By the serial on the label, in decimal, though the registry and FB11 spell it in hex.
    let out = e.remove(&NodeRemoveTarget::Serial(" 2162165722 ".into()));
    assert!(out.success, "{out:?}");

    assert_eq!(
        radio.ran(),
        vec![
            Ran::Remove {
                radio: "p2p".into(),
                dev_eui: "5876070000000902".into()
            },
            Ran::Remove {
                radio: "p2p".into(),
                dev_eui: "5876070000005722".into()
            },
        ]
    );
    assert!(radio.registry().nodes.is_empty(), "both left the registry");
    assert_eq!(e.list().expect("answers"), vec![]);
    // The removal's tombstone went out, as for the Portal's.
    assert!(radio
        .info("5876070000000902")
        .last()
        .is_some_and(|p| p.is_empty()));
}

/// The serial FB11 lists (8 hex) addresses the Node as well as the label's decimal, and a Node
/// on a carrier this Hub has switched off is removed on the carrier it is on.
#[test]
fn fb12_by_the_listed_serial_reaches_a_node_on_a_switched_off_carrier() {
    let radio = Radio::new("p2p");
    radio.seed_lorawan("5876070000000413", "2162164755", Some("Cold store"));
    let e = enroll(&radio);

    let out = e.remove(&NodeRemoveTarget::Serial("80e00413".into()));
    assert!(out.success, "{out:?}");
    assert_eq!(
        radio.ran(),
        vec![Ran::Remove {
            radio: "lorawan".into(),
            dev_eui: "5876070000000413".into()
        }]
    );
}

/// Nothing is removed unless exactly one Node matches. A record migrated with a serial that is
/// not a STICKER serial is matched as typed.
#[test]
fn fb12_refuses_an_unknown_or_ambiguous_target_and_matches_a_legacy_serial_as_typed() {
    let radio = Radio::new("lorawan");
    radio.seed_lorawan("5876070000000001", "2162164755", None);
    radio.seed_lorawan("5876070000000002", "2162164755", None);
    radio.seed_lorawan("5876070000000003", "SN-003", Some("Old label"));
    let e = enroll(&radio);

    let out = e.remove(&NodeRemoveTarget::Serial("2162164755".into()));
    assert!(!out.success, "{out:?}");
    assert!(out.message.contains("matches 2 enrolled nodes"), "{out:?}");
    for target in [
        NodeRemoveTarget::Serial("4242".into()),
        NodeRemoveTarget::DevEui("5876070000000009".into()),
    ] {
        let out = e.remove(&target);
        assert!(!out.success, "{out:?}");
        assert!(out.message.contains("no node is enrolled with"), "{out:?}");
    }
    assert!(radio.ran().is_empty(), "nothing was removed");

    let out = e.remove(&NodeRemoveTarget::Serial(" SN-003".into()));
    assert!(out.success, "{out:?}");
    assert_eq!(
        radio.ran(),
        vec![Ran::Remove {
            radio: "lorawan".into(),
            dev_eui: "5876070000000003".into()
        }]
    );
}

/// On a LoRaWAN Hub FB0D sends the Portal's LoRaWAN `node-add`. An all-zero JoinEUI is "not
/// given", so the Hub's `radio.join_eui` applies, as for a blank Portal field; a real one goes
/// through. `profile_id` (v1's ChirpStack profile number) is not the v2 profile.
#[test]
fn fb0d_on_a_lorawan_hub_leaves_an_all_zero_join_eui_to_the_hub() {
    let radio = Radio::new("lorawan");
    let e = enroll(&radio);

    let r = e.add(&fb0d("5876070000000413", "2162164755", "Cold store"));
    assert!(r.success, "{r:?}");
    let mut with_join = fb0d("5876070000000414", "0x80e00414", "Dock");
    with_join.joineui = "58760700000000AB".into();
    with_join.profile_id = Some(7);
    assert!(e.add(&with_join).success);

    assert_eq!(
        radio.ran(),
        vec![
            Ran::Add {
                radio: "lorawan".into(),
                profile: "default".into(),
                serial: "2162164755".into(),
                dev_eui: "5876070000000413".into(),
                name: Some("Cold store".into()),
                join_eui: None,
            },
            Ran::Add {
                radio: "lorawan".into(),
                profile: "default".into(),
                serial: "0x80e00414".into(),
                dev_eui: "5876070000000414".into(),
                name: Some("Dock".into()),
                join_eui: Some("58760700000000ab".into()),
            },
        ]
    );
    assert_eq!(
        listed(&e),
        vec![
            entry("5876070000000413", "80e00413", "Cold store"),
            entry("5876070000000414", "80e00414", "Dock"),
        ]
    );
}

/// A `dual` Hub registers on P2P, the carrier the Portal's dialog preselects there, and a P2P
/// registration carries no JoinEUI whatever the tag says.
#[test]
fn fb0d_on_a_dual_hub_registers_on_p2p_without_a_join_eui() {
    let radio = Radio::new("dual");
    let e = enroll(&radio);
    let mut req = fb0d("5876070000000902", "2162190902", "Cold room");
    req.joineui = "58760700000000AB".into();

    let r = e.add(&req);
    assert!(r.success, "{r:?}");
    assert!(
        matches!(&radio.ran()[..], [Ran::Add { radio, join_eui: None, .. }] if radio == "p2p"),
        "{:?}",
        radio.ran()
    );
    assert_eq!(
        radio.registry().get("5876070000000902").map(|n| n.radio),
        Some(Transport::P2p)
    );
}

/// One carrier per Node (decision #23): a DevEUI the registry holds on the other carrier is
/// refused before anything runs, naming that carrier. The LoRaWAN arm would otherwise move it.
#[test]
fn fb0d_refuses_a_node_registered_on_the_other_carrier() {
    let radio = Radio::new("dual");
    radio.seed_lorawan("5876070000000413", "2162164755", Some("Cold store"));
    let before = radio.registry();
    let e = enroll(&radio);

    let r = e.add(&fb0d("5876070000000413", "2162164755", "Cold store"));
    assert!(!r.success && !r.pending, "{r:?}");
    assert!(
        r.message.contains("already registered on LoRaWAN"),
        "{}",
        r.message
    );
    assert!(r.message.contains("Change radio"), "{}", r.message);
    assert!(radio.ran().is_empty());
    assert_eq!(radio.registry(), before, "nothing moved");
}

/// A serial the registry cannot canonicalise is refused, as the Portal refuses it: a v1 label
/// such as `SN-1` is no STICKER serial.
#[test]
fn fb0d_refuses_a_serial_that_is_not_a_sticker_serial() {
    let radio = Radio::new("p2p");
    let e = enroll(&radio);
    let r = e.add(&fb0d("5876070000000902", "SN-1", "Cold room"));
    assert!(!r.success, "{r:?}");
    assert!(r.message.contains("serial_number"), "{}", r.message);
    assert!(radio.ran().is_empty());
}

/// `radio.mode: off` leaves the legacy stack on (`radio_gate`), so the three verbs keep the
/// legacy path: FB11 reads `lorawan.sensors[]`, not the registry, and FB0D answers with the
/// legacy path's own refusal for a Hub with no config applier.
#[test]
fn mode_off_keeps_the_legacy_path() {
    let radio = Radio::new("off");
    radio.seed_lorawan("5876070000000413", "2162164755", Some("Registry only"));
    std::fs::write(
        radio.dir.path().join("fiber.config.yaml"),
        "lorawan:\n  sensors:\n    - dev_eui: 0011223344556677\n      serial_number: SN-001\n      name: Fridge 1\n",
    )
    .expect("seed");
    let e = enroll(&radio);

    assert_eq!(
        listed(&e),
        vec![entry("0011223344556677", "SN-001", "Fridge 1")]
    );
    let r = e.add(&fb0d("5876070000000902", "2162190902", "Cold room"));
    assert!(!r.success);
    assert!(r.message.contains("config applier"), "{}", r.message);
    assert!(radio.ran().is_empty(), "the registry path was not taken");
}
