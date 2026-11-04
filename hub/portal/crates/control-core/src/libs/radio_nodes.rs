//! The signed `node-add`, `node-remove` and `node-move` commands — plan/portal/control.radio.md
//! §7, the MQTT twins of `proximosctl control.radio node-add` / `node-remove` / `node-move`.
//!
//! All three call the functions `control_radio::dispatch` runs for the CLI, so an enrolment from
//! the portal and one from the CLI are the same enrolment: same ChirpStack objects, same registry
//! record, same retained `info` document and tombstones. The commands exist
//! because the portal has no other way in — the control socket is `proximosctl`'s transport
//! only (decision #10), and the legacy `add_lorawan_sticker` arm is exactly what
//! [`crate::libs::radio_gate`] refuses on a Hub whose `radio.mode` names a transport.
//!
//! The ChirpStack client is built from the tenant token in the radio store, the way
//! `proximosctl` builds it. `fiber.service` runs as root until the monolith lands, so the
//! 0600 token is readable; the store hands a `nodes.json` it creates back to the directory's
//! owner (`control_radio::store`), so `proximos` can still read it afterwards.

use control_radio::cli::{Command, ControlRadio, NodeIdent, NodeMoveArgs, RadioArg};
use control_radio::config::RadioConfig;
use control_radio::dispatch::{
    enroll_lorawan_node, load_config, needs_chirpstack, remove_lorawan_node, store_root, Outcome,
};
use control_radio::lorawan::client::ChirpStack;
use control_radio::lorawan::enroll::{EnrollRequest, NodeRef};
use control_radio::lorawan::keyfile::{self, KeyMaterial};
use control_radio::lorawan::tonic_client::TonicChirpStack;
use control_radio::lorawan::{Eui, Tokens};
use control_radio::mqtt::RumqttcPublisher;
use control_radio::northbound::RetainedPublisher;
use control_radio::store::Store;
use control_radio::Transport;

/// What the command asks for, once `authorization::manager` has checked the shape.
pub enum Op {
    Add {
        /// `"lorawan"` or `"p2p"`: the carrier the Node is registered on.
        radio: String,
        /// A `device_profiles.otaa.<name>` key of the rendered radio config.
        profile: String,
        serial: String,
        dev_eui: String,
        name: Option<String>,
        /// Hex, 32 chars. OTAA is the only enrollable activation in phase 1.
        app_key: String,
        /// The operator's JoinEUI (#86). `None` leaves it to `enroll_lorawan_node`, which
        /// applies `radio.join_eui` — the same default the CLI uses without `--join-eui`.
        join_eui: Option<String>,
    },
    Remove {
        /// The carrier the registry holds the Node on.
        radio: String,
        dev_eui: String,
    },
    /// Put the Node on the other carrier (node_registry.md §4): the CLI's
    /// `node-move --dev-eui <dev_eui> --to <to>`. `to` is a target, not a `--radio` arm, so the
    /// mode gate below does not apply; `control_radio::node_move` runs its own.
    Move {
        dev_eui: String,
        /// `"lorawan"` or `"p2p"`.
        to: String,
    },
}

/// Execute one command on the `radio` arm. The `Err` text is the command response's
/// `error_message`, so it carries `control_radio`'s own error line (kind, component, message)
/// rather than a paraphrase — a portal shows it verbatim, and it is the only sentence that
/// says what to do.
pub fn run(op: Op) -> Result<(), String> {
    run_at(
        op,
        std::path::Path::new(control_radio::cluster_role::CLUSTER_DIR),
    )
}

/// [`run`] with the cluster directory injected, so a test can stand in a Client.
fn run_at(op: Op, cluster_dir: &std::path::Path) -> Result<(), String> {
    // A cluster Client registers, removes and moves no Nodes: they belong to the Server
    // (control.radio.md, amendment 2026-10-13). First, before the config gate, so a Client
    // answers with the one reason that matters. Nothing is relayed to the Server.
    control_radio::cluster_role::refuse_on_client_at(cluster_dir).map_err(|e| e.to_string())?;
    // The gate, first: no rendered `radio:` section means this Hub has no v2 radio stack, and
    // the legacy `add_lorawan_sticker` / `remove_lorawan_sticker` arms are what serve it.
    let config = load_config().map_err(|e| {
        format!("this Hub has no v2 radio configuration ({e}); use the legacy sticker commands")
    })?;
    let radio = match op {
        // No carrier gate: the target of a move may be a carrier this Hub has switched off.
        Op::Move { dev_eui, to } => return run_move(&config, &dev_eui, &to),
        Op::Add { ref radio, .. } | Op::Remove { ref radio, .. } => radio.clone(),
    };
    let transport = match radio.as_str() {
        "lorawan" => Transport::Lorawan,
        "p2p" => Transport::P2p,
        other => {
            return Err(format!(
                "unknown radio {other:?}: expected \"lorawan\" or \"p2p\""
            ))
        }
    };
    if !config.mode.allows(transport) {
        // A Node on a carrier this Hub has switched off can still be removed (node_registry.md
        // §4, amended 2026-09-27): control-radio's `node-remove` does it without the carrier.
        if let Op::Remove { dev_eui, .. } = &op {
            let publisher = RumqttcPublisher::new();
            return run_cli(
                &config,
                &[
                    "node-remove",
                    "--radio",
                    transport.as_str(),
                    "--dev-eui",
                    dev_eui,
                ],
                Store::new(store_root()),
                Some(&publisher),
            );
        }
        return Err(format!(
            "radio.mode={} has no {} arm",
            config.mode.as_str(),
            transport.as_str()
        ));
    }
    if transport == Transport::P2p {
        let publisher = RumqttcPublisher::new();
        return run_p2p(&config, op, Store::new(store_root()), Some(&publisher));
    }

    // Its own thread and its own runtime: this runs inside the MQTT monitor's runtime, where
    // `block_on` would panic, and the work is a handful of gRPC round trips the legacy arms
    // blocked the command loop for too.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("cannot start the async runtime: {e}"))?;
                rt.block_on(async {
                    let store = Store::new(store_root());
                    let client = TonicChirpStack::connect(Tokens::new(store.clone()))
                        .await
                        .map_err(|e| e.to_string())?;
                    let publisher = RumqttcPublisher::new();
                    let result = match op {
                        Op::Add {
                            profile,
                            serial,
                            dev_eui,
                            name,
                            app_key,
                            join_eui,
                            ..
                        } => {
                            let dev_eui = Eui::parse(&dev_eui).map_err(|e| e.to_string())?;
                            // The same parser the CLI runs on a keys file, so "32 hex chars"
                            // is checked in one place.
                            let material: KeyMaterial = keyfile::parse(
                                &serde_json::json!({ "app_key": app_key }).to_string(),
                            )
                            .map_err(|e| e.to_string())?;
                            // Canonical form through the same `Eui` the DevEUI takes, so the
                            // record and ChirpStack see one spelling of it.
                            let join_eui = join_eui
                                .map(|j| Eui::parse(&j).map(|e| e.as_str().to_string()))
                                .transpose()
                                .map_err(|e| e.to_string())?;
                            enroll_lorawan_node(
                                EnrollRequest {
                                    serial: &serial,
                                    dev_eui,
                                    profile: &profile,
                                    otaa: true,
                                    join_eui,
                                    name,
                                    material,
                                },
                                &config,
                                &client,
                                &publisher,
                                &store,
                            )
                            .await
                        }
                        Op::Remove { dev_eui, .. } => {
                            let dev_eui = Eui::parse(&dev_eui).map_err(|e| e.to_string())?;
                            remove_lorawan_node(
                                NodeRef::DevEui(dev_eui.clone()),
                                dev_eui,
                                &config,
                                &client,
                                &publisher,
                                &store,
                            )
                            .await
                        }
                        Op::Move { .. } => unreachable!("handled before the carrier gate"),
                    }
                    .map_err(|e| e.to_string())?;
                    // The §5 result envelope, the line `proximosctl` would have printed.
                    eprintln!(
                        "[radio_nodes] {}",
                        serde_json::to_string(&result).unwrap_or_default()
                    );
                    Ok(())
                })
            })
            .join()
            .map_err(|_| "the radio command thread panicked".to_string())?
    })
}

/// The P2P arm: the CLI's own `node-add --radio p2p` / `node-remove --radio p2p`, run in
/// process through `control_radio::dispatch`, so a registration from the portal and one from
/// `proximosctl` are the same registration — the same registry record, retained `info` and
/// tombstones. Until 2026-09-27 this arm answered "phase 2" although the CLI had registered P2P
/// Nodes for weeks, so the portal could neither register nor remove one (night test
/// 2026-09-26, PN-8).
///
/// The key rides in a 0600 file the CLI's `--keys-file` reads, created fresh in the temporary
/// directory and removed when this returns — never on an argument list, never logged.
///
/// Public so a test of a caller of [`run`] can drive this shipped arm against a temporary store
/// and a fake broker (`tests/ble_nodes_registry.rs`, GitLab #118); [`run`] is what callers call.
pub fn run_p2p(
    config: &RadioConfig,
    op: Op,
    store: Store,
    publisher: Option<&dyn RetainedPublisher>,
) -> Result<(), String> {
    use clap::Parser;

    let keys = match &op {
        Op::Add { app_key, .. } => Some(TempKeys::write(app_key)?),
        Op::Remove { .. } | Op::Move { .. } => None,
    };
    let mut args: Vec<String> = vec!["control.radio".into()];
    match &op {
        Op::Add {
            profile,
            serial,
            dev_eui,
            name,
            ..
        } => {
            args.extend(
                [
                    "node-add",
                    "--radio",
                    "p2p",
                    "--profile",
                    profile,
                    "--serial",
                    serial,
                    "--dev-eui",
                    dev_eui,
                ]
                .map(String::from),
            );
            if let Some(name) = name {
                args.extend(["--name".to_string(), name.clone()]);
            }
            if let Some(keys) = &keys {
                args.extend(["--keys-file".to_string(), keys.path.display().to_string()]);
            }
        }
        Op::Remove { dev_eui, .. } => {
            args.extend(["node-remove", "--radio", "p2p", "--dev-eui", dev_eui].map(String::from))
        }
        Op::Move { .. } => return Err("node-move has no carrier arm".to_string()),
    }
    let cli = control_radio::cli::ControlRadioCli::try_parse_from(args)
        .map_err(|e| format!("cannot build the P2P command: {e}"))?;

    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("cannot start the async runtime: {e}"))?;
                rt.block_on(async {
                    let outcome = control_radio::dispatch::run_with_store(
                        &cli.inner, config, None, None, publisher, store,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    if let control_radio::dispatch::Outcome::Json(v) = outcome {
                        eprintln!("[radio_nodes] {v}");
                    }
                    Ok(())
                })
            })
            .join()
            .map_err(|_| "the radio command thread panicked".to_string())?
    })
}

/// The signed `northbridge-update` (`plan/control/radio/northbridge_update.md` §3.4, amended
/// 2026-10-09): the CLI's own `northbridge-update --radio p2p --no-wait [--force]`, in process.
///
/// The bundled image only: no file travels over MQTT. `--no-wait`, because the flash takes up to
/// a minute and the MQTT command loop answers in seconds; the command is done once the request
/// is written, and the outcome reaches the portal in the retained `radio/northbridge`
/// document's `update` object. The CLI's own checks answer: no bundled firmware, a mode without
/// P2P, no P2P task running — each with `control_radio`'s sentence.
pub fn northbridge_update(force: bool) -> Result<(), String> {
    let config = load_config().map_err(|e| {
        format!("this Hub has no v2 radio configuration ({e}); there is no Northbridge to update")
    })?;
    run_cli(
        &config,
        &northbridge_update_args(force),
        Store::new(store_root()),
        None,
    )
}

/// The CLI arguments of [`northbridge_update`].
fn northbridge_update_args(force: bool) -> Vec<&'static str> {
    let mut args = vec!["northbridge-update", "--radio", "p2p", "--no-wait"];
    if force {
        args.push("--force");
    }
    args
}

/// One CLI verb through `control_radio::dispatch`, with no ChirpStack client: for a verb whose
/// arm is not needed, such as `node-remove` on a switched-off carrier.
fn run_cli(
    config: &control_radio::config::RadioConfig,
    verb: &[&str],
    store: Store,
    publisher: Option<&dyn RetainedPublisher>,
) -> Result<(), String> {
    use clap::Parser;
    let mut args = vec!["control.radio"];
    args.extend_from_slice(verb);
    let cli = control_radio::cli::ControlRadioCli::try_parse_from(args)
        .map_err(|e| format!("cannot build the command: {e}"))?;
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("cannot start the async runtime: {e}"))?;
                rt.block_on(async {
                    let outcome = control_radio::dispatch::run_with_store(
                        &cli.inner, config, None, None, publisher, store,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    if let control_radio::dispatch::Outcome::Json(v) = outcome {
                        eprintln!("[radio_nodes] {v}");
                    }
                    Ok(())
                })
            })
            .join()
            .map_err(|_| "the radio command thread panicked".to_string())?
    })
}

/// `node-move`: the CLI's own verb through `control_radio::dispatch`, with typed arguments — a
/// move carries no key, so there is no argument list and no key file to hand it.
///
/// ChirpStack is connected wherever `needs_chirpstack` says the CLI would connect, which for a
/// move is wherever LoRaWAN is on: either end of a move may be LoRaWAN's. A connection that fails
/// refuses the move before anything changed, as the CLI's does (node_registry.md §10 Q2).
fn run_move(config: &RadioConfig, dev_eui: &str, to: &str) -> Result<(), String> {
    let cli = move_cli(dev_eui, to)?;
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("cannot start the async runtime: {e}"))?;
                rt.block_on(async {
                    let store = Store::new(store_root());
                    let client = if needs_chirpstack(&cli, config) {
                        Some(
                            TonicChirpStack::connect(Tokens::new(store.clone()))
                                .await
                                .map_err(|e| e.to_string())?,
                        )
                    } else {
                        None
                    };
                    let publisher = RumqttcPublisher::new();
                    move_node(
                        config,
                        &cli,
                        client.as_ref().map(|c| c as &dyn ChirpStack),
                        Some(&publisher),
                        store,
                    )
                    .await
                    .map(|_| ())
                })
            })
            .join()
            .map_err(|_| "the radio command thread panicked".to_string())?
    })
}

/// The CLI invocation `node-move --dev-eui <dev_eui> --to <to>`, built typed.
fn move_cli(dev_eui: &str, to: &str) -> Result<ControlRadio, String> {
    let to = match to {
        "lorawan" => RadioArg::Lorawan,
        "p2p" => RadioArg::P2p,
        other => {
            return Err(format!(
                "unknown radio {other:?}: expected \"lorawan\" or \"p2p\""
            ))
        }
    };
    Ok(ControlRadio {
        json: true,
        command: Command::NodeMove(NodeMoveArgs {
            ident: NodeIdent {
                serial: None,
                dev_eui: Some(dev_eui.to_string()),
            },
            to,
        }),
    })
}

/// The move itself, the ChirpStack client and the broker injected so a test drives the shipped
/// path against fakes. Returns the §5 result envelope (`from`, `to`, `state`, `next_step`).
async fn move_node(
    config: &RadioConfig,
    cli: &ControlRadio,
    client: Option<&dyn ChirpStack>,
    publisher: Option<&dyn RetainedPublisher>,
    store: Store,
) -> Result<serde_json::Value, String> {
    let outcome =
        control_radio::dispatch::run_with_store(cli, config, client, None, publisher, store)
            .await
            .map_err(|e| e.to_string())?;
    let result = match outcome {
        Outcome::Json(v) => v,
        Outcome::Text(t) => serde_json::json!({ "message": t }),
        Outcome::Status(_) => serde_json::json!({}),
    };
    eprintln!("[radio_nodes] {result}");
    Ok(result)
}

/// The `{ "app_key": … }` file `--keys-file` reads: 0600, created new, removed on drop.
struct TempKeys {
    path: std::path::PathBuf,
}

impl TempKeys {
    fn write(app_key: &str) -> Result<Self, String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "radio-node-add-{}-{nanos}.json",
            std::process::id()
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("cannot create the key file for node-add: {e}"))?;
        let keys = Self { path };
        file.write_all(
            serde_json::json!({ "app_key": app_key })
                .to_string()
                .as_bytes(),
        )
        .map_err(|e| format!("cannot write the key file for node-add: {e}"))?;
        Ok(keys)
    }
}

impl Drop for TempKeys {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RADIO_SECTION: &str =
        include_str!("../../../control-radio/tests/fixtures/radio-section.yaml");

    fn p2p_config() -> control_radio::config::RadioConfig {
        let yaml = RADIO_SECTION.replacen("mode: lorawan", "mode: p2p", 1);
        control_radio::config::RadioConfig::from_device_config_yaml(&yaml).expect("p2p config")
    }

    /// A cluster Client refuses all three verbs before anything is read or written, naming
    /// its Server (control.radio.md, amendment 2026-10-13). Nothing reaches the radio store.
    #[test]
    fn a_cluster_client_refuses_node_add_remove_and_move() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("role"), "client").unwrap();
        std::fs::write(dir.path().join("server_host"), "proximos-4e970f4a").unwrap();
        let ops = [
            Op::Add {
                radio: "p2p".into(),
                profile: "default".into(),
                serial: "2162190902".into(),
                dev_eui: "5876070000000902".into(),
                name: None,
                app_key: "00112233445566778899aabbccddeeff".into(),
                join_eui: None,
            },
            Op::Remove {
                radio: "lorawan".into(),
                dev_eui: "5876070000000902".into(),
            },
            Op::Move {
                dev_eui: "5876070000000902".into(),
                to: "p2p".into(),
            },
        ];
        for op in ops {
            let err = run_at(op, dir.path()).expect_err("a Client refuses");
            assert!(
                err.contains(
                    "this Hub is a cluster Client; Nodes are registered on the Server proximos-4e970f4a"
                ),
                "{err}"
            );
        }
    }

    /// PN-8 (night test 2026-09-26): the signed `node-add` / `node-remove` of a P2P Node are
    /// the CLI's own verbs — the registry gets the record, name included, and loses it again.
    #[test]
    fn a_signed_p2p_registration_and_removal_reach_the_registry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::new(dir.path());
        let config = p2p_config();
        let broker = control_radio::lorawan::fake::FakePublisher::new();

        run_p2p(
            &config,
            Op::Add {
                radio: "p2p".into(),
                profile: "default".into(),
                serial: "2162190902".into(),
                dev_eui: "5876070000000902".into(),
                name: Some("Cold room".into()),
                app_key: "00112233445566778899aabbccddeeff".into(),
                join_eui: None,
            },
            store.clone(),
            Some(&broker),
        )
        .expect("registered");
        let db = control_radio::p2p::db::SessionsDb::load(&store).expect("registry");
        let node = db.node("5876070000000902").expect("the record");
        assert_eq!(node.serial, "80e06a36");
        assert_eq!(node.name.as_deref(), Some("Cold room"));

        run_p2p(
            &config,
            Op::Remove {
                radio: "p2p".into(),
                dev_eui: "5876070000000902".into(),
            },
            store.clone(),
            Some(&broker),
        )
        .expect("removed");
        let db = control_radio::p2p::db::SessionsDb::load(&store).expect("registry");
        assert!(db.node("5876070000000902").is_none());
        // The registration's `info` and the removal's tombstone went out, as from the CLI.
        let info: Vec<_> = broker
            .published()
            .into_iter()
            .filter(|(t, _)| t.ends_with("/nodes/5876070000000902/info"))
            .collect();
        assert!(info.len() >= 2, "{info:?}");
        assert!(
            info.last().is_some_and(|(_, p)| p.is_empty()),
            "tombstone last"
        );
    }

    /// Registers `dev_eui` over P2P through the signed `node-add`, as the portal does.
    fn register_p2p(store: &Store, config: &RadioConfig, dev_eui: &str) {
        let broker = control_radio::lorawan::fake::FakePublisher::new();
        run_p2p(
            config,
            Op::Add {
                radio: "p2p".into(),
                profile: "default".into(),
                serial: "2162165722".into(),
                dev_eui: dev_eui.into(),
                name: None,
                app_key: "00112233445566778899aabbccddeeff".into(),
                join_eui: None,
            },
            store.clone(),
            Some(&broker),
        )
        .expect("registered");
    }

    fn radio_of(store: &Store, dev_eui: &str) -> Option<Transport> {
        control_radio::registry::Registry::load(store)
            .expect("registry")
            .and_then(|r| r.get(dev_eui).map(|n| n.radio))
    }

    fn run_move(
        config: &RadioConfig,
        store: &Store,
        dev_eui: &str,
        to: &str,
        publisher: Option<&dyn RetainedPublisher>,
    ) -> Result<serde_json::Value, String> {
        let cli = move_cli(dev_eui, to)?;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(move_node(config, &cli, None, publisher, store.clone()))
    }

    /// node_registry.md §4 through the signed `node-move`: the registry's `radio` flips, the
    /// retained `info` goes out under the new carrier, and the way back restores it. On a P2P
    /// Hub the LoRaWAN end needs no ChirpStack: the arm is off, and its mirror catches up when
    /// it is switched on.
    #[test]
    fn a_signed_move_changes_the_nodes_carrier_and_back() {
        const DEV: &str = "5876070000005722";
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::new(dir.path());
        let config = p2p_config();
        register_p2p(&store, &config, DEV);
        assert_eq!(radio_of(&store, DEV), Some(Transport::P2p));

        let broker = control_radio::lorawan::fake::FakePublisher::new();
        let result = run_move(&config, &store, DEV, "lorawan", Some(&broker)).expect("moved");
        assert_eq!(result["from"], "p2p");
        assert_eq!(result["to"], "lorawan");
        assert_eq!(result["state"], "moved");
        assert!(
            result["next_step"]
                .as_str()
                .is_some_and(|s| s.contains("radio-mode")),
            "{result}"
        );
        assert_eq!(radio_of(&store, DEV), Some(Transport::Lorawan));
        let info = broker
            .last_payload(
                &control_radio::northbound::Topics::new(control_radio::northbound::hostname())
                    .node(DEV, control_radio::northbound::Area::Info),
            )
            .expect("info republished");
        let info: serde_json::Value = serde_json::from_slice(&info).expect("json");
        assert_eq!(info["transport"], "lorawan");

        run_move(&config, &store, DEV, "p2p", Some(&broker)).expect("moved back");
        assert_eq!(radio_of(&store, DEV), Some(Transport::P2p));
    }

    /// A move is refused, with nothing changed, when it cannot republish the retained `info`, and
    /// a target that is not a carrier never reaches the registry.
    #[test]
    fn a_move_that_cannot_complete_changes_nothing() {
        const DEV: &str = "5876070000005722";
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::new(dir.path());
        let config = p2p_config();
        register_p2p(&store, &config, DEV);

        let err = run_move(&config, &store, DEV, "lorawan", None).expect_err("no broker");
        assert!(err.contains("broker"), "{err}");
        let err = run_move(&config, &store, DEV, "wifi", None).expect_err("not a carrier");
        assert!(err.contains("wifi"), "{err}");
        let err = run_move(&config, &store, "5876070000000001", "lorawan", None)
            .expect_err("no such Node");
        assert!(err.to_lowercase().contains("no node"), "{err}");
        assert_eq!(radio_of(&store, DEV), Some(Transport::P2p), "nothing moved");
    }

    /// northbridge_update.md §3.4 (amended 2026-10-09): the signed command is the CLI's
    /// `northbridge-update --radio p2p --no-wait`, `--force` only when asked; never `--file`.
    #[test]
    fn the_signed_northbridge_update_is_the_clis_bundled_no_wait_request() {
        use clap::Parser;
        for force in [false, true] {
            let args = northbridge_update_args(force);
            assert!(!args.contains(&"--file") && !args.contains(&"--manifest"));
            let cli = control_radio::cli::ControlRadioCli::try_parse_from(
                std::iter::once("control.radio").chain(args.iter().copied()),
            )
            .expect("the CLI parses it");
            match cli.inner.command {
                Command::NorthbridgeUpdate(a) => {
                    assert_eq!(a.force, force);
                    assert!(a.no_wait, "the command loop does not wait out a flash");
                    assert!(a.file.is_none() && a.manifest.is_none(), "bundled only");
                    assert_eq!(a.radio.transport(), Some(Transport::P2p));
                }
                other => panic!("expected northbridge-update, got {other:?}"),
            }
        }
    }

    /// The verb runs through the CLI's own gate: a Hub without P2P refuses it before anything
    /// is written to the radio store.
    #[test]
    fn a_northbridge_update_on_a_hub_without_p2p_is_refused_and_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::new(dir.path());
        let config = control_radio::config::RadioConfig::from_device_config_yaml(RADIO_SECTION)
            .expect("lorawan config");
        assert!(
            !config.mode.allows(Transport::P2p),
            "the fixture is LoRaWAN only"
        );
        let err =
            run_cli(&config, &northbridge_update_args(false), store, None).expect_err("no P2P arm");
        assert!(err.to_lowercase().contains("p2p"), "{err}");
        assert!(
            !dir.path().join("northbridge").exists(),
            "no request was written"
        );
    }

    #[test]
    fn the_key_file_is_private_and_goes_away() {
        use std::os::unix::fs::PermissionsExt;
        let path = {
            let keys = TempKeys::write("00112233445566778899aabbccddeeff").expect("written");
            let meta = std::fs::metadata(&keys.path).expect("exists");
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
            keys.path.clone()
        };
        assert!(!path.exists(), "removed on drop");
    }
}
