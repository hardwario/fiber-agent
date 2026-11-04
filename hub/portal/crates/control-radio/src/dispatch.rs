//! Command dispatch — `plan/control/radio/plan.md` §1, §5.
//!
//! One entry point. `radio.mode` is checked **before anything else**: before argument
//! validation, before any file is opened, before any RPC. Everything downstream can then assume
//! it is running on an arm the Hub actually has.

use crate::cache::{Cache, CacheSlot};
use crate::cli::{Command, ControlRadio, NodeAddArgs, NodeIdent, NodeSendArgs, NodeSendKind};
use crate::config::RadioConfig;
use crate::decode::command_encode;
use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::client::ChirpStack;
use crate::lorawan::converge::{Converger, LorawanReport};
use crate::lorawan::enroll::{
    EnrollRequest, Enroller, EnrollmentRecord, KeyDisclosure, NodeRef, NodesFile,
};
use crate::lorawan::eui::Eui;
use crate::lorawan::gateway::{self, ApiStatus, GatewaysReport, DISCOVERY_TIMEOUT};
use crate::lorawan::keyfile::{self, KeyMaterial};
use crate::lorawan::state::{Activation, ConvergedState};
use crate::lorawan::token::Tokens;
use crate::mode::{resolve, Mode};
use crate::northbound::{self, Area, RetainedPublisher, Topics};
use crate::status::{CommandResult, P2pReport, SpreadingFactorReport, StatusReport};
use crate::store::{Slot, Store};

/// Where the renderer puts the device config. `PROXIMOS_CONFIG` overrides it for the dev loop
/// and the integration stack, matching the portal's `PORTAL_CONFIG` convention.
pub const DEFAULT_CONFIG_PATH: &str = "/run/proximos/control/config.yaml";

pub fn config_path() -> String {
    std::env::var("PROXIMOS_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string())
}

pub fn store_root() -> String {
    std::env::var("PROXIMOS_RADIO_DATA").unwrap_or_else(|_| crate::store::DEFAULT_ROOT.to_string())
}

pub fn load_config() -> Result<RadioConfig> {
    let path = config_path();
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        Error::new(
            Kind::NotBootstrapped,
            None,
            Component::ConfigJson,
            format!("cannot read the device config at {path}: {e}"),
        )
    })?;
    RadioConfig::from_device_config_yaml(&raw)
}

/// What a command produced. Rendering — `--json` or human text — is the caller's.
#[derive(Debug)]
pub enum Outcome {
    Status(Box<StatusReport>),
    Json(serde_json::Value),
    Text(String),
}

/// Run one invocation.
///
/// `client` is a trait object so the integration stack can drive the same dispatch path against
/// a live ChirpStack and the unit tests against the in-memory fake — the code under test is the
/// shipped code, not a parallel arrangement.
///
/// It is **optional** because a P2P-only Hub runs no ChirpStack at all. Connecting eagerly
/// would make every command on such a Hub fail `ServiceDown` for a service it does not have,
/// which is the opposite of what the mode gate is for. Callers use [`needs_chirpstack`] to
/// decide whether to connect.
///
/// `topics` and `publisher` are the broker's two halves — the retained *read* discovery uses
/// and the retained *write* `node-add`/`node-remove` owe §9.4 — injected separately for the
/// same reason `client` is: a unit test proves the tombstone without a broker, and the
/// integration stack drives the identical code against a real one.
pub async fn run(
    cli: &ControlRadio,
    config: &RadioConfig,
    client: Option<&dyn ChirpStack>,
    topics: Option<&dyn gateway::RetainedTopics>,
    publisher: Option<&dyn RetainedPublisher>,
) -> Result<Outcome> {
    run_with_store(
        cli,
        config,
        client,
        topics,
        publisher,
        Store::new(store_root()),
    )
    .await
}

/// [`run`] with the component state tree injected instead of resolved from
/// `PROXIMOS_RADIO_DATA`. The binaries never call this; it exists so the CLI-level behaviour
/// (`status` while ChirpStack is down, `node-remove`, `gateway-remove`) is testable against the
/// in-memory fake and a temp dir without touching process-global environment.
pub async fn run_with_store(
    cli: &ControlRadio,
    config: &RadioConfig,
    client: Option<&dyn ChirpStack>,
    topics: Option<&dyn gateway::RetainedTopics>,
    publisher: Option<&dyn RetainedPublisher>,
    store: Store,
) -> Result<Outcome> {
    let tokens = Tokens::new(store.clone());

    // Argument constraints clap cannot express. Before the gate, because a malformed
    // invocation is a usage error whatever `radio.mode` happens to be.
    cli.command.validate()?;

    // A cluster Client registers no Nodes: they belong to the Server (control.radio.md,
    // amendment 2026-10-13). Before every other gate, so the refusal names the real reason.
    if crate::cluster_role::is_node_registration(&cli.command) {
        crate::cluster_role::refuse_on_client()?;
    }

    // `node-move` names a target carrier, not a `--radio` arm, and may target one this Hub has
    // switched off (node_registry.md §4), so it runs its own gate.
    if let Command::NodeMove(args) = &cli.command {
        return crate::node_move::run(config, &store, args, client, publisher).await;
    }
    // A Node on a carrier this Hub has switched off can still be removed (node_registry.md §4,
    // amended 2026-09-27); the carrier's own arm, which the gate below refuses, is not needed.
    if let Command::NodeRemove(args) = &cli.command {
        if let Some(outcome) =
            crate::node_move::remove_on_switched_off_carrier(config, &store, &args.ident, publisher)
                .await?
        {
            return Ok(outcome);
        }
    }

    // `create --role client` is the cluster join. It passes no mode gate: a Client runs the
    // Server's mode, which the Server sets after the join (decision #47), so a fresh `off` Hub
    // joins too. `--radio` only labels the outcome.
    if let Command::Create(args) = &cli.command {
        if args.role_client() {
            return create_client(args.radio.transport().unwrap_or(Transport::Lorawan), args);
        }
    }

    // The boot unit runs `create` with no `--radio` (plan.md §5, "Boot invocation"). Under
    // `dual` that is the one place the dual-ambiguity rule does not apply: both arms run,
    // LoRaWAN converge then P2P init, each with its own outcome.
    if let Command::Create(args) = &cli.command {
        if config.mode == Mode::Dual && args.radio.transport().is_none() {
            let lorawan = create_arm(
                Transport::Lorawan,
                args,
                config,
                client,
                topics,
                &tokens,
                &store,
            )
            .await;
            let p2p = create_arm(
                Transport::P2p,
                args,
                config,
                client,
                topics,
                &tokens,
                &store,
            )
            .await;
            // A failed arm fails the run, so the boot unit retries it. The other arm's outcome
            // goes to the journal rather than being lost.
            return match (lorawan, p2p) {
                (Ok(lorawan), Ok(p2p)) => Ok(Outcome::Json(
                    serde_json::json!({ "lorawan": lorawan, "p2p": p2p }),
                )),
                (Err(e), Ok(other)) | (Ok(other), Err(e)) => {
                    eprintln!("[create] the other arm: {other}");
                    Err(e)
                }
                (Err(e), Err(other)) => {
                    eprintln!("[create] p2p: {other}");
                    Err(e)
                }
            };
        }
    }

    // The gate, first. Everything below may assume the arm exists.
    let transport = resolve(
        config.mode,
        cli.command.requested_transport(),
        cli.command.radio_arity(),
    )?;

    // Resolved lazily: only the arms that talk to ChirpStack ever unwrap this. A P2P-only Hub
    // runs no ChirpStack, so connecting eagerly would fail every command with ServiceDown for
    // a service it does not have — the opposite of what the mode gate is for.
    let owned_config = config.clone();
    let need_client = || -> Result<&dyn ChirpStack> {
        client.ok_or_else(|| {
            Error::new(
                Kind::ServiceDown,
                Transport::Lorawan,
                Component::ChirpstackService,
                "no ChirpStack connection: this command needs the LoRaWAN arm".to_string(),
            )
        })
    };

    // The write half of the broker seam, resolved the same lazily-and-only-where-needed way.
    // Only `node-add` and `node-remove` need it: §9.4 makes the retained `info` document and
    // its tombstone part of what those two commands *are*, not a side effect something else
    // catches up on later.
    let need_publisher = |transport: Transport| -> Result<&dyn RetainedPublisher> {
        publisher.ok_or_else(|| {
            Error::new(
                Kind::ServiceDown,
                transport,
                Component::MqttBroker,
                "no broker connection: this command has to publish the retained node \
                 inventory (plan.md §9.4)"
                    .to_string(),
            )
        })
    };

    match &cli.command {
        Command::Status(_) => {
            // `daemon.running` is `proximos-control`'s heartbeat — the same freshness rule
            // `node-get` gates on. It was a hard-coded `false` while nothing hosted the P2P
            // central (P2P-N8); since PI1 the one resident process hosts both arms.
            let mut report = StatusReport::new(
                config.mode,
                crate::tasks::heartbeat::is_fresh(crate::tasks::health::interval()),
            );
            if config.mode.allows(Transport::Lorawan) {
                // A degraded read: report what is on disk rather than failing because
                // ChirpStack is down. `status` must work when things are broken — that is when
                // it is used. `.ok()` used to drop the whole block on any RPC error, which
                // threw away token presence and the boot marker: facts that are on disk and
                // still true when the API is not answering (F10).
                let provisioned = store.is_provisioned(Transport::Lorawan);
                report.lorawan = Some(match client {
                    Some(c) => {
                        let conv = Converger {
                            client: c,
                            config: &owned_config,
                            tokens: &tokens,
                            store: &store,
                        };
                        match conv.create(None, true).await {
                            Ok(r) => r,
                            Err(e) => LorawanReport::degraded(
                                tokens.presence_report(),
                                provisioned,
                                ApiStatus::from_error(&e),
                                e.to_string(),
                            ),
                        }
                    }
                    None => LorawanReport::degraded(
                        tokens.presence_report(),
                        provisioned,
                        ApiStatus::Unreachable,
                        "no ChirpStack connection".to_string(),
                    ),
                });
            }
            if config.mode.allows(Transport::P2p) {
                // A degraded read, like the LoRaWAN block above: an unreadable session DB must
                // not take the whole report with it — `status` is what an operator runs when
                // something is broken. An unreadable one reads as "no override", and
                // `session_db` above is the key that says the file itself is the problem.
                let radio_override = crate::p2p::db::SessionsDb::load(&store)
                    .unwrap_or_default()
                    .radio
                    .map(|r| r.spreading_factor);
                report.p2p = Some(P2pReport {
                    initialized: store.exists(Slot::P2pNetId),
                    provisioned: store.is_provisioned(Transport::P2p),
                    net_id: store.read(Slot::P2pNetId)?.map(|s| s.trim().to_string()),
                    session_db: if store.exists(Slot::P2pSessions) {
                        "ok"
                    } else {
                        "absent"
                    },
                    spreading_factor: config.p2p_radio.as_ref().map(|p| {
                        SpreadingFactorReport::from_parts(p.spreading_factor, radio_override)
                    }),
                    northbridge: read_run_document("northbridge.json"),
                });
            }
            Ok(Outcome::Status(Box::new(report)))
        }

        Command::Create(args) => create_arm(
            transport,
            args,
            &owned_config,
            client,
            topics,
            &tokens,
            &store,
        )
        .await
        .map(Outcome::Json),

        Command::Wipe(args) | Command::Clear(args) => {
            if !args.force {
                return Err(Error::new(
                    Kind::InvalidArgument,
                    transport,
                    Component::ControlSocket,
                    "--force is required: this deletes enrolled devices".to_string(),
                ));
            }
            if transport == Transport::P2p {
                // §5 table: `clear` deletes the enrolled devices (registry + sessions + cache)
                // and keeps `net_id`/provisioning; `wipe` also removes `net_id` and the
                // provisioned marker, so the next `create` regenerates. Both are re-runnable.
                let wipe = matches!(cli.command, Command::Wipe(_));
                // Counted and removed under the lock, so a concurrent runtime save cannot
                // recreate the file between the two.
                let _db_lock = store.lock(Slot::P2pSessions)?;
                let db = crate::p2p::db::SessionsDb::load(&store)?;
                let (nodes, sessions) = (db.nodes.len() as u64, db.sessions.len() as u64);
                let ids: Vec<String> = db.nodes.keys().cloned().collect();
                // The registry's P2P Nodes are what `nodes` counted (node_registry.md); the file
                // only mirrors them.
                crate::registry::drop_carrier(&store, Transport::P2p)?;
                // `clear` keeps the gateway TX counter floor with the `net_id` it belongs to:
                // a counter handed out twice under one `net_id` repeats a nonce. `wipe` takes
                // both (p2p_tower_gateway.md §3.4).
                if wipe {
                    store.remove(Slot::P2pSessions)?;
                } else {
                    crate::p2p::db::SessionsDb::clear_keeping_counter(&store)?;
                }
                // The same rule the LoRaWAN arm below follows, applied to the same shared
                // cache: decision #4 moved the P2P `node-get` documents out of `sessions.db`
                // and into `p2p/cache/`, so removing the one slot no longer takes them with
                // it. After the devices are gone, not before.
                Cache::new(&store, Transport::P2p).remove_all(&ids)?;
                if wipe {
                    store.remove(Slot::P2pNetId)?;
                    store.clear_provisioned(Transport::P2p)?;
                }
                return Ok(Outcome::Json(
                    serde_json::to_value(
                        CommandResult::new(if wipe { "wipe" } else { "clear" }, Transport::P2p)
                            .with("nodes_removed", nodes)
                            .with("sessions_removed", sessions)
                            .with("net_id", if wipe { "removed" } else { "kept" }),
                    )
                    .map_err(json_err)?,
                ));
            }
            let c = need_client()?;
            let conv = Converger {
                client: c,
                config: &owned_config,
                tokens: &tokens,
                store: &store,
            };
            // One cache for both carriers (node_registry.md §3): only the LoRaWAN Nodes' go.
            let ids: Vec<String> = crate::registry::current(&store)?
                .on(Transport::Lorawan)
                .map(|n| n.dev_eui.clone())
                .collect();
            let report = if matches!(cli.command, Command::Wipe(_)) {
                conv.wipe().await?
            } else {
                conv.clear().await?
            };
            // "the `node-get` cache … survives process restarts, **wiped by `clear`/`wipe`**"
            // (plan.md, Runtime form). After the devices are gone, not before: a failed delete
            // must leave the operator's `node-get` still answering about a device that is
            // demonstrably still enrolled.
            Cache::new(&store, Transport::Lorawan).remove_all(&ids)?;
            // And the P2P sessions those Nodes parked when they moved here (node_registry.md
            // §4, amended 2026-09-28), which no registry record would reach any more.
            crate::p2p::db::drop_parked_or_log(&store, &ids);
            Ok(Outcome::Json(
                serde_json::to_value(report).map_err(json_err)?,
            ))
        }

        Command::GatewayScan(_) => {
            let t = topics.ok_or_else(|| {
                Error::new(
                    Kind::ServiceDown,
                    Transport::Lorawan,
                    Component::MqttBroker,
                    "no broker connection to read retained gateway state from".to_string(),
                )
            })?;
            match gateway::discover(t, &forwarder_topic_prefix(config), DISCOVERY_TIMEOUT).await? {
                Some(eui) => Ok(Outcome::Text(eui.as_str().to_string())),
                None => Ok(Outcome::Text(String::new())),
            }
        }

        Command::GatewayAdd(args) => {
            if transport == Transport::P2p {
                return Err(phase2(
                    "a P2P gateway is a Hub; phase 1's single-Hub topology registers nothing",
                ));
            }
            // Validate the EUI before anything else, so a typo is a local InvalidArgument
            // rather than a round trip.
            let eui = Eui::parse(args.gateway_eui.as_deref().unwrap_or_default())?;
            let name = args
                .name
                .clone()
                .unwrap_or_else(|| config.gateway.name.clone());

            // The gateway-bridge opt-in flag is a renderer input that does not exist yet
            // (lorawan.md §7 names it "once it exists"). Until it does, external registration
            // is refused with BridgeDisabled rather than registering a gateway that can never
            // connect: with the bridge off, port 1700 never opens.
            let bridge_enabled = std::env::var("PROXIMOS_GATEWAY_BRIDGE_ENABLED")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);

            let c = need_client()?;
            let conv = Converger {
                client: c,
                config: &owned_config,
                tokens: &tokens,
                store: &store,
            };
            let tenant_id = conv.resolve_tenant_id().await?;
            let existed = gateway::add_external(c, &tenant_id, &eui, &name, bridge_enabled).await?;
            Ok(Outcome::Json(
                serde_json::to_value(
                    crate::status::CommandResult::new("gateway-add", Transport::Lorawan)
                        .with("gateway_eui", eui.as_str())
                        .with("name", name)
                        .with(
                            "state",
                            if existed {
                                "already-registered"
                            } else {
                                "registered"
                            },
                        ),
                )
                .map_err(json_err)?,
            ))
        }

        Command::NodeAdd(args) => {
            if transport == Transport::P2p {
                // §5: P2P `node-add` is pure registration and the bundle is **required** — the
                // pairing window that used to make it optional is withdrawn (p2p.md §8). The
                // bundle check is local, so it runs first: a partial bundle is `InvalidArgument`
                // before any registry write, naming the caller's missing argument rather than
                // the registry.
                let dev_eui = p2p_registration_bundle(args, transport)?;
                refuse_if_on_the_other_carrier(config, &store, &dev_eui, transport)?;
                // The serial is a registration *attribute* since #73 — the registry key is
                // the `dev_eui` resolved above. It is still canonicalised, because `--serial`
                // remains an operator-facing alias that resolves through the registry, and an
                // alias that only matched one spelling of the number printed on the device
                // would be worse than no alias. A non-numeric serial cannot be a P2P device.
                let serial = crate::p2p::session::canonical_serial(
                    args.serial.as_deref().unwrap_or_default(),
                )
                .ok_or_else(|| {
                    Error::new(
                        Kind::InvalidArgument,
                        transport,
                        Component::ControlSocket,
                        "node-add --radio p2p --serial must be the device's numeric serial \
                         (decimal, 0x-hex, or 8 hex digits)",
                    )
                })?;
                // The canonical form from the bundle check, never `args.dev_eui` — see
                // `p2p_registration_bundle` for what the raw spelling would cost.
                let dev_eui = dev_eui.as_str().to_owned();
                // P2P registers an `app_key` (the same OTAA-shaped `{ "app_key": … }`); ABP
                // key material is LoRaWAN-only. Re-tag any keyfile error onto P2P (§6).
                let app_key = match read_key_material(args, transport).map_err(|mut e| {
                    e.0.transport = Some(transport);
                    e
                })? {
                    KeyMaterial::Otaa(k) => k.app_key,
                    KeyMaterial::Abp(_) => {
                        return Err(Error::new(
                            Kind::InvalidArgument,
                            transport,
                            Component::EnrollmentRecord,
                            "P2P key material must be an app_key; ABP keys are LoRaWAN-only",
                        ));
                    }
                };
                // The existence check and the write are one critical section: two concurrent
                // node-adds for the same serial would both see an empty registry otherwise.
                let _db_lock = store.lock(Slot::P2pSessions)?;
                let mut db = crate::p2p::db::SessionsDb::load(&store)?;
                // §6: registration is add-only. An identity already in the registry is
                // `AlreadyExists` (`component: node-registry`), never a silent overwrite — the
                // caller must `node-remove` it first and re-add, which is also the path an
                // `app_key` rotation takes (`p2p.md` §10, decided 2026-08-31). The existence
                // check precedes the write, mirroring LoRaWAN's ask-first node-add.
                //
                // Both keys are checked. The dev_eui is the registry key (#73); the serial is
                // an attribute, but `--serial` still resolves through it, so letting one serial
                // sit under two dev_euis would make that alias ambiguous — a `node-send
                // --serial` would then reach whichever record the scan happened to find first.
                if db.node(&dev_eui).is_some() {
                    return Err(Error::new(
                        Kind::AlreadyExists,
                        transport,
                        Component::NodeRegistry,
                        format!(
                            "dev_eui {dev_eui} is already registered; node-remove it first to \
                             re-register (an app_key rotation is a remove-then-add)"
                        ),
                    ));
                }
                if let Some(other) = db.node_by_serial(&serial) {
                    return Err(Error::new(
                        Kind::AlreadyExists,
                        transport,
                        Component::NodeRegistry,
                        format!(
                            "serial {serial} is already registered under dev_eui {}; \
                             node-remove that record first",
                            other.dev_eui
                        ),
                    ));
                }
                // The P2P gateway keys its peers by the DevEUI's low 32 bits, so a second
                // Node on the same address could never be served (p2p_tower_gateway.md §3.1).
                crate::p2p::tower::check_address_free(
                    &dev_eui,
                    db.nodes.keys().map(String::as_str),
                )?;
                // A fresh registration inherits nothing from the last one. A session can
                // outlive a `node-remove` (it carries the pending Detach) or a `clear` that was
                // never run, and its `dev_nonce` high-water would refuse the new node's first
                // JoinRequest as a replay.
                db.sessions.remove(&dev_eui);
                db.upsert_node(crate::p2p::session::RegisteredNode {
                    serial: serial.clone(),
                    dev_eui: dev_eui.clone(),
                    profile: args.profile.clone(),
                    app_key: hex_lower(&app_key),
                    name: args
                        .name
                        .as_deref()
                        .map(str::trim)
                        .filter(|n| !n.is_empty())
                        .map(str::to_string),
                });
                db.save(&store)?;

                // §9.4: publish the retained `info` inventory document so a late subscriber —
                // the portal included — sees the node the moment it is registered, not only
                // once it first joins on air (the runtime publishes the richer joined document
                // then). Read the record back so the document provably describes what the
                // registry holds, and release the session lock before the broker round trip —
                // a network publish is not something to hold a cross-process lock across.
                // Best-effort about the broker *existing* (a P2P registration is otherwise
                // fully local), but an error from a broker that does exist is surfaced, the
                // same posture the node-remove tombstone takes.
                let registered = db.node(&dev_eui).cloned();
                drop(_db_lock);
                if let (Some(broker), Some(node)) = (publisher, registered) {
                    let mut info = node.info_core();
                    info.insert(
                        "pairing_status".to_string(),
                        serde_json::Value::String("registered".to_string()),
                    );
                    let topic = Topics::new(northbound::hostname()).node(&dev_eui, Area::Info);
                    let payload =
                        serde_json::to_vec(&serde_json::Value::Object(info)).map_err(json_err)?;
                    broker.publish_retained(&[(topic, payload)]).await?;
                }

                return Ok(Outcome::Json(
                    serde_json::to_value(
                        CommandResult::new("node-add", Transport::P2p)
                            .with("serial_number", serial)
                            .with("dev_eui", dev_eui)
                            .with("profile", args.profile.clone()),
                    )
                    .map_err(json_err)?,
                ));
            }
            let otaa = match (args.otaa, args.abp) {
                (true, false) => true,
                (false, true) => false,
                _ => {
                    return Err(Error::new(
                        Kind::InvalidArgument,
                        transport,
                        Component::ControlSocket,
                        "node-add --radio lorawan needs exactly one of --otaa or --abp",
                    ));
                }
            };
            let serial = args.serial.as_deref().ok_or_else(|| {
                Error::new(
                    Kind::InvalidArgument,
                    transport,
                    Component::ControlSocket,
                    "node-add --radio lorawan needs --serial",
                )
            })?;
            let dev_eui_arg = args.dev_eui.as_deref().ok_or_else(|| {
                Error::new(
                    Kind::InvalidArgument,
                    transport,
                    Component::ControlSocket,
                    "node-add --radio lorawan needs --dev-eui",
                )
            })?;
            let dev_eui = Eui::parse(dev_eui_arg)?;
            refuse_if_on_the_other_carrier(config, &store, &dev_eui, transport)?;
            let material = read_key_material(args, transport)?;

            let c = need_client()?;
            // Resolved before the enrollment, so "there is no broker" fails the command
            // without having created a device whose inventory entry can never be published.
            let broker = need_publisher(Transport::Lorawan)?;
            let result = enroll_lorawan_node(
                EnrollRequest {
                    serial,
                    dev_eui,
                    profile: &args.profile,
                    otaa,
                    join_eui: args.join_eui.clone(),
                    name: args.name.clone(),
                    material,
                },
                &owned_config,
                c,
                broker,
                &store,
            )
            .await?;
            Ok(Outcome::Json(
                serde_json::to_value(result).map_err(json_err)?,
            ))
        }

        Command::GatewayRemove(args) => {
            if transport == Transport::P2p {
                return Err(phase2(
                    "a P2P gateway is a Hub; phase 1's single-Hub topology registers nothing",
                ));
            }
            // Same order as gateway-add: a typo is a local InvalidArgument, not a round trip.
            let eui = Eui::parse(args.gateway_eui.as_deref().unwrap_or_default())?;

            let c = need_client()?;
            let conv = Converger {
                client: c,
                config: &owned_config,
                tokens: &tokens,
                store: &store,
            };
            // A gateway that is not there is already in the desired state — the same
            // tolerate-missing rule `wipe` uses for deletes.
            match c.delete_gateway(&eui).await {
                Ok(()) => {}
                Err(e) if e.kind() == Kind::NotFound => {}
                Err(e) => return Err(e),
            }
            conv.forget_gateway(&eui)?;

            Ok(Outcome::Json(
                serde_json::to_value(
                    CommandResult::new("gateway-remove", Transport::Lorawan)
                        .with("gateway_eui", eui.as_str())
                        .with("state", "deleted"),
                )
                .map_err(json_err)?,
            ))
        }

        Command::NodeRemove(args) => {
            if transport == Transport::P2p {
                let _db_lock = store.lock(Slot::P2pSessions)?;
                let mut db = crate::p2p::db::SessionsDb::load(&store)?;
                // The one resolver, same as every other P2P arm — it also proves the node is
                // registered, so `remove_node` below cannot miss.
                let dev_eui = resolve_p2p_node(&db, &args.ident, "node-remove")?;
                // The Node's session ends with its registration: the central takes the address
                // off the gateway's peer table on its next pass (p2p_tower_gateway.md §3.4), or
                // once its Detach is delivered (p2p_control_commands.md §3), and nothing it
                // sends under this session is published again. That makes this arm the only
                // chance to clear its retained `info`, so the tombstone is part of what the
                // command *is*, and the broker is demanded before anything is mutated: once
                // the registry entry is gone the identifier the operator typed no longer
                // resolves and a re-run answers `NotFound`, leaving the Node in every
                // subscriber's inventory forever. The LoRaWAN arm has always resolved its
                // publisher first, for this reason.
                let broker = need_publisher(Transport::P2p)?;
                // H-1 (p2p_control_commands.md §3): a joined Node that understands `Detach` is
                // told to go silent before it leaves the gateway. Its session stays, detaching,
                // until the central has delivered the Detach.
                let detach = db.sessions.get(&dev_eui).is_some_and(|s| {
                    crate::p2p::tower::control::gate(
                        s.capabilities.as_ref(),
                        crate::p2p::tower::control::cmd::DETACH,
                    )
                    .is_ok()
                });
                let (removed, _session) = db.remove_node(&dev_eui);
                let serial = removed.map(|n| n.serial).unwrap_or_default();
                let state = if detach {
                    let now = crate::lorawan::gateway::now_unix();
                    if let Some(s) = db.sessions.get_mut(&dev_eui) {
                        s.downlink_queue.clear();
                        s.downlink_phase = crate::p2p::session::DownlinkPhase::Idle;
                        s.history = None;
                        s.downlink_queue
                            .push_back(crate::p2p::session::QueuedDownlink::control(
                                crate::p2p::tower::control::ControlDownlink::Detach {
                                    reason: crate::p2p::tower::control::DETACH_REASON_REMOVED,
                                },
                            ));
                        s.detach = Some(crate::p2p::session::Detaching {
                            since: crate::lorawan::gateway::format_rfc3339(now),
                            since_unix: u64::try_from(now).unwrap_or(0),
                        });
                    }
                    "detaching"
                } else {
                    // The join anti-replay history goes with it: the same hardware registered
                    // again joins from scratch, whatever `dev_nonce` it restarts at.
                    db.sessions.remove(&dev_eui);
                    "deleted"
                };
                // Tombstones first, while the registry entry is still on disk: a broker that
                // cannot be reached then leaves the node registered and the whole command
                // re-runnable by the identifier the operator typed. Publishing after the save
                // would strand a deregistered node whose retained `info` nobody can clear.
                {
                    let stones: Vec<(String, Vec<u8>)> = Topics::new(northbound::hostname())
                        .tombstones(&dev_eui)
                        .into_iter()
                        .map(|t| (t, northbound::TOMBSTONE.to_vec()))
                        .collect();
                    let names = stones
                        .iter()
                        .map(|(t, _)| t.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    broker.publish_retained(&stones).await.map_err(|e| {
                        Error::new(
                            Kind::ServiceDown,
                            Transport::P2p,
                            Component::MqttBroker,
                            format!(
                                "the registry entry for {dev_eui} is still present, but the \
                                 retained {names} could not be cleared: {}",
                                e.message()
                            ),
                        )
                    })?;
                }
                // The cached radio-reported data goes with the record, and before it — the
                // same ordering the LoRaWAN arm below uses, for the same reason: while the
                // registry entry is still there the whole command can be re-run by the
                // identifier the operator typed.
                Cache::new(&store, Transport::P2p).remove(&dev_eui)?;
                db.save(&store)?;
                return Ok(Outcome::Json(
                    serde_json::to_value(
                        CommandResult::new("node-remove", Transport::P2p)
                            .with("serial_number", serial)
                            .with("dev_eui", dev_eui)
                            .with("state", state),
                    )
                    .map_err(json_err)?,
                ));
            }
            let node = node_ref(&args.ident, transport, "node-remove")?;
            // Resolved locally first: an unknown `--serial` is a bookkeeping miss, and saying
            // so before touching ChirpStack or the broker keeps the diagnosis about the
            // argument the operator typed. §5's asymmetric corner survives — a `--dev-eui`
            // with no record resolves to itself and addresses ChirpStack directly.
            let dev_eui = node.resolve(&NodesFile::load(&store)?)?;
            let c = need_client()?;
            let broker = need_publisher(Transport::Lorawan)?;
            let result =
                remove_lorawan_node(node, dev_eui, &owned_config, c, broker, &store).await?;
            Ok(Outcome::Json(
                serde_json::to_value(result).map_err(json_err)?,
            ))
        }

        Command::NodeInfo(args) => {
            if transport == Transport::P2p {
                let db = crate::p2p::db::SessionsDb::load(&store)?;
                let dev_eui = resolve_p2p_node(&db, &args.ident, "node-info")?;
                let node = db.node(&dev_eui).ok_or_else(|| {
                    Error::new(
                        Kind::NotFound,
                        transport,
                        Component::NodeRegistry,
                        format!("no P2P node registered with dev_eui {dev_eui}"),
                    )
                })?;
                let sess = db.sessions.get(&dev_eui);
                // The registration record — local bookkeeping — plus session state, in §5's
                // golden-baseline vocabulary so a consumer reads a P2P node as the same kind of
                // object as a LoRaWAN one. `net_id` is read rather than required: a Hub that has
                // not run `create` has none, and that is worth reporting as null rather than
                // failing a read-only command.
                //
                // No key material, as on neither arm's `node-info` today, and no
                // `pairing_status` until §5's vocabulary for it is settled.
                let mut info = serde_json::json!({
                    "serial_number": node.serial,
                    "dev_eui": node.dev_eui,
                    "transport": Transport::P2p.as_str(),
                    "profile": node.profile,
                    // The Node's P2P address, the low 32 bits of its DevEUI (contract §3.1).
                    "dev_addr": sess.map(|s| s.addr),
                    "net_id": store.read(Slot::P2pNetId)?.map(|s| s.trim().to_string()),
                    "session_state": sess.map(|_| crate::p2p::session::SessionState::Joined),
                    // The newest counter the gateway accepted from the Node.
                    "uplink_counter": sess.map(|s| s.last_seen),
                    "dev_nonce": sess.map(|s| s.dev_nonce),
                    "last_join_at": sess.and_then(|s| s.joined_at.clone()),
                    "downlink_pending": sess.map(|s| s.downlink_queue.len()),
                });
                // The operator's name when one was registered, as the LoRaWAN arm reports it
                // (PN-6): present when set, absent otherwise.
                if let (Some(map), Some(name)) = (info.as_object_mut(), node.name.as_ref()) {
                    map.insert("name".into(), name.clone().into());
                }
                return Ok(Outcome::Json(info));
            }
            // §5: this command is `control_radio`'s **own bookkeeping**, never a radio read
            // and — deliberately — never a ChirpStack read either. It therefore answers on a
            // Hub whose ChirpStack is down, which is when an operator most wants to check what
            // a device was provisioned with.
            let node = node_ref(&args.ident, transport, "node-info")?;
            let nodes = NodesFile::load(&store)?;
            let record = nodes.find(&node).ok_or_else(|| {
                Error::new(
                    Kind::NotFound,
                    Transport::Lorawan,
                    Component::EnrollmentRecord,
                    format!(
                        "no enrollment record for {}: node-info reports this component's own \
                         bookkeeping, so a device that was enrolled elsewhere has none",
                        describe(&node)
                    ),
                )
            })?;
            // Plaintext key material, on purpose — §5 decides it explicitly ("the operational
            // need to verify a key actually matches what was provisioned … outweighs the
            // exposure concern"), and this path is CLI/control-socket only: `node-info` has no
            // BLE characteristic at all.
            let mut info = record.to_json(KeyDisclosure::Included);
            // A Node moved here from P2P keeps its P2P session, parked (node_registry.md §4,
            // amended 2026-09-28, Q-N4). Said, so an operator can tell a move back will be
            // taken up at once. Absent otherwise.
            if crate::p2p::db::SessionsDb::has_parked_session(&store, record.dev_eui.as_str()) {
                if let Some(map) = info.as_object_mut() {
                    map.insert("p2p_session".into(), "parked".into());
                }
            }
            Ok(Outcome::Json(info))
        }

        Command::NodeList(_) => {
            if transport == Transport::P2p {
                let db = crate::p2p::db::SessionsDb::load(&store)?;
                let net_id = store.read(Slot::P2pNetId)?.map(|s| s.trim().to_string());
                let cache = Cache::new(&store, Transport::P2p);
                let nodes: Vec<serde_json::Value> = db
                    .nodes
                    .values()
                    .map(|n| {
                        let sess = db.sessions.get(&n.dev_eui);
                        // `last_seen` is the newest thing the radio reported, which on this arm
                        // is the cache the uplink path fills — there is no ChirpStack to ask.
                        // `gateways` is null for the same reason it is on the LoRaWAN arm: §5's
                        // per-gateway rssi/snr rows are not in any record a list command reads.
                        let last_seen = cache.read(&n.dev_eui).ok().flatten().and_then(|doc| {
                            CacheSlot::ALL
                                .iter()
                                .filter_map(|s| doc.slot(*s).map(|e| e.at))
                                .max()
                        });
                        let mut row = serde_json::json!({
                            "serial_number": n.serial,
                            "dev_eui": n.dev_eui,
                            "transport": Transport::P2p.as_str(),
                            "profile": n.profile,
                            "dev_addr": sess.map(|s| s.addr),
                            "net_id": net_id,
                            "session_state": sess.map(|_| crate::p2p::session::SessionState::Joined),
                            "last_seen": last_seen,
                            "gateways": serde_json::Value::Null,
                        });
                        if let (Some(map), Some(name)) = (row.as_object_mut(), n.name.as_ref()) {
                            map.insert("name".into(), name.clone().into());
                        }
                        row
                    })
                    .collect();
                // §5's baseline is the bare list; the `{command, radio}` envelope was this arm's
                // own invention and no LoRaWAN list carries one.
                return Ok(Outcome::Json(serde_json::json!({ "nodes": nodes })));
            }
            let c = need_client()?;
            let conv = Converger {
                client: c,
                config: &owned_config,
                tokens: &tokens,
                store: &store,
            };
            // ChirpStack is the authority on which devices exist; `nodes.json` is the
            // authority on what this Hub enrolled them as. Listing from the API and joining
            // the records is what makes a device enrolled out of band visible at all — it
            // appears without a `serial_number`, which is exactly the fact an operator needs.
            let state = conv.load_state()?;
            let app_id = state.application_id()?.to_string();
            let devices = c.list_devices(&app_id).await?;
            let nodes = NodesFile::load(&store)?;

            let mut rows: Vec<serde_json::Value> = devices
                .iter()
                .map(|d| node_list_row(d, nodes.by_dev_eui(&d.dev_eui), &state))
                .collect();
            // Deterministic order: the API's order is its own business, and this output is a
            // golden baseline scripts diff.
            rows.sort_by(|a, b| a["dev_eui"].as_str().cmp(&b["dev_eui"].as_str()));

            Ok(Outcome::Json(serde_json::json!({ "nodes": rows })))
        }

        Command::GatewayList(_) => {
            if transport == Transport::P2p {
                return Err(phase2(
                    "the P2P row of radio/gateways is this Hub's own northbridge health, \
                     which the agent owns",
                ));
            }
            let c = need_client()?;
            let conv = Converger {
                client: c,
                config: &owned_config,
                tokens: &tokens,
                store: &store,
            };
            let tenant_id = tenant_for_read(&conv).await?;
            let gateways = c.list_gateways(&tenant_id).await?;
            let now = gateway::now_unix();

            // `api_status` is `ok` because the call this report is built from succeeded: a
            // failure surfaces as the §6 error envelope, which names the failing component
            // rather than degrading the whole list into "everything offline" — the v1 defect
            // §6 calls out. The retained-topic-sourced view §6 describes for the *published*
            // `radio/gateways` belongs to the health task (WP-D); on demand, the API's answer
            // is the freshest thing available.
            Ok(Outcome::Json(
                serde_json::to_value(GatewaysReport {
                    timestamp: gateway::format_rfc3339(now),
                    api_status: ApiStatus::Ok,
                    gateways: gateways
                        .iter()
                        .map(|g| gateway::to_status(g, now))
                        .collect(),
                })
                .map_err(json_err)?,
            ))
        }

        Command::GatewayInfo(args) => {
            if transport == Transport::P2p {
                return Err(phase2(
                    "the P2P gateway record is this Hub itself; nothing was ever registered",
                ));
            }
            // Same order as the other gateway commands: a typo is a local InvalidArgument.
            let eui = Eui::parse(args.gateway_eui.as_deref().unwrap_or_default())?;
            let c = need_client()?;
            let conv = Converger {
                client: c,
                config: &owned_config,
                tokens: &tokens,
                store: &store,
            };
            let tenant_id = tenant_for_read(&conv).await?;

            // Through the tenant-scoped list, not `GatewayService.Get`, for the reason F20
            // documents: 4.19.0 authorizes `Get` with an EXISTS-join, so an unregistered EUI
            // answers `Unauthenticated` under every key below admin. `gateway-info` on a
            // gateway that is not registered has to say `NotFound` — that is the whole answer
            // the operator asked for — and only the list can say it.
            let found = c.find_gateway(&tenant_id, &eui).await?.ok_or_else(|| {
                Error::new(
                    Kind::NotFound,
                    Transport::Lorawan,
                    Component::ChirpstackApi,
                    format!("gateway {eui} is not registered in this tenant"),
                )
            })?;

            let now = gateway::now_unix();
            let mut doc =
                serde_json::to_value(gateway::to_status(&found, now)).map_err(json_err)?;
            // §5: `gateway-info` is one `radio/gateways` row **plus the registration record**
            // — the two ChirpStack-only fields that have no P2P counterpart.
            if let Some(map) = doc.as_object_mut() {
                map.insert("tenant_id".to_string(), found.tenant_id.clone().into());
                map.insert("created_at".to_string(), found.created_at.clone().into());
            }
            Ok(Outcome::Json(doc))
        }

        Command::NodeSend(args) => {
            // `0x91` control commands take their own short path (p2p_control_commands.md §2).
            if let (Transport::P2p, Some(ctl)) = (
                transport,
                args.what.as_ref().and_then(NodeSendKind::control),
            ) {
                return send_p2p_control(&store, &args.ident, ctl);
            }
            if transport == Transport::P2p {
                // Both forms build their bytes *before* anything is resolved or queued, for the
                // same reason the LoRaWAN arm below does: a wrong payload costs an RX window and
                // a duty-cycle slot, and the operator finds out minutes later, if at all.
                let planned = match &args.what {
                    Some(kind) => Some(command_encode::plan(kind, transport)?),
                    None => None,
                };

                // The whole enqueue is one critical section against the runtime's event
                // handler: this is the write the 2026-08-28 bench lost.
                let _db_lock = store.lock(Slot::P2pSessions)?;
                let mut db = crate::p2p::db::SessionsDb::load(&store)?;
                // Resolves *and* proves registration — an unregistered node never reaches the
                // session check below.
                let dev_eui = resolve_p2p_node(&db, &args.ident, "node-send")?;
                let serial = db
                    .node(&dev_eui)
                    .map(|n| n.serial.clone())
                    .unwrap_or_default();
                // Requires a live session: a P2P downlink is sealed under the session key and
                // addressed to the session's dev_addr, so there is nothing to encrypt it against
                // until the node has joined. The runtime delivers it on the node's next uplink
                // (announce -> 0x56 -> 0x55 clear).
                let no_session = || {
                    Error::new(
                        Kind::NotFound,
                        transport,
                        Component::SessionDb,
                        format!(
                            "no live P2P session for {dev_eui} — the node has not joined, so \
                             there is no session key to seal a downlink under"
                        ),
                    )
                };
                let too_long = |n: usize| {
                    Error::new(
                        Kind::InvalidArgument,
                        transport,
                        Component::ControlSocket,
                        format!(
                            "node-send payload is {n} B, past the {} B a single P2P downlink carries \
                             (transport-side fragmentation is a follow-up)",
                            crate::p2p::tower::MAX_COMMAND_BODY
                        ),
                    )
                };

                // Q-N1 (plan.md §5, amended 2026-09-28): an identical idempotent ask still
                // waiting in the queue is withdrawn for this one. A head already delivered stays
                // (`waiting_downlinks`), and this frame queues behind it.
                let coalescing = match &planned {
                    Some(p) => coalescing_kind(p, transport)?,
                    None => None,
                };
                // Q20b (config_sync.md §13): a Node's whole pending configuration withdraws an
                // earlier one still waiting, the same way.
                let supersede = superseding(args);
                let withdraw: Vec<u32> = if coalescing.is_some() || supersede {
                    let sent = Cache::new(&store, transport).read(dev_eui.as_str())?;
                    db.waiting_downlinks(&dev_eui)
                        .into_iter()
                        .filter(|(seq, frame)| match coalescing {
                            Some(kind) => {
                                crate::coalesce::replaces(kind, *seq, frame, sent.as_ref())
                            }
                            None => crate::coalesce::replaces_config(*seq, frame, sent.as_ref()),
                        })
                        .map(|(seq, _)| seq)
                        .collect()
                } else {
                    Vec::new()
                };

                // A P2P downlink only moves when the node transmits, so an unbounded queue turns
                // a scripting mistake into a backlog the operator waits out one uplink at a time.
                // Counted for the whole request, so a multi-frame `set-config` is refused whole
                // rather than half-queued. What this request withdraws makes room.
                let wanted = planned.as_ref().map_or(1, |p| p.downlinks.len());
                let queued = db
                    .queued_downlinks(&dev_eui)
                    .ok_or_else(no_session)?
                    .saturating_sub(withdraw.len());
                if queued + wanted > crate::p2p::db::MAX_QUEUED_DOWNLINKS {
                    return Err(Error::new(
                        Kind::InvalidArgument,
                        transport,
                        Component::SessionDb,
                        format!(
                            "{dev_eui} already has {queued} downlink(s) queued and this adds \
                             {wanted}, past the {} a node may have waiting; let the queue drain \
                             or node-remove it",
                            crate::p2p::db::MAX_QUEUED_DOWNLINKS
                        ),
                    ));
                }

                // §5's baseline: the addressed identifier, the wire identity always (the caller
                // may only have held a serial), and the `queued` boolean the LoRaWAN arm answers
                // with — `state: "queued"` says the same thing in prose, but a consumer written
                // against the baseline branches on the boolean.
                let result = CommandResult::new("node-send", Transport::P2p)
                    .with("serial_number", serial.clone())
                    .with("dev_eui", dev_eui.as_str())
                    .with("queued", true)
                    .with("state", "queued");

                let Some(planned) = planned else {
                    // A P2P frame has no port field: the 0x56 body *is* an fPort-85 command
                    // (`p2p.md` §6.2), so 85 is the only honest answer to `--fport`. clap already
                    // requires the flag with `--hex`; refusing any other value here keeps the
                    // operator from believing a port was carried.
                    let (f_port, data) = raw_downlink(args, transport)?;
                    if f_port != command_encode::COMMAND_FPORT {
                        return Err(Error::new(
                            Kind::InvalidArgument,
                            transport,
                            Component::ControlSocket,
                            format!(
                                "P2P carries fPort-{p} commands only; use --fport {p}",
                                p = command_encode::COMMAND_FPORT
                            ),
                        ));
                    }
                    if data.len() > crate::p2p::tower::MAX_COMMAND_BODY {
                        return Err(too_long(data.len()));
                    }
                    let depth = db
                        .enqueue_downlink(&dev_eui, crate::p2p::session::QueuedDownlink::raw(data))
                        .ok_or_else(no_session)?;
                    db.save(&store)?;
                    return Ok(Outcome::Json(
                        serde_json::to_value(
                            result
                                .with("type", "hex")
                                .with("fport", f_port)
                                .with("queue_depth", depth as u64),
                        )
                        .map_err(json_err)?,
                    ));
                };

                // The structured forms are fPort 85 by protocol and carry §9.7's `seq` inside the
                // body, allocated from the node's own persistent high-water mark — the CLI process
                // exits, so it cannot live in memory.
                let cache = Cache::new(&store, transport);
                // Out of the session queue the way a Discard's draft goes; only saved with the new
                // frame below, so a refusal on the way leaves the queue as it was.
                if !withdraw.is_empty() {
                    db.drop_queued_seqs(&dev_eui, &withdraw);
                }
                let mut sent = Vec::new();
                let mut depth = 0usize;
                for (i, downlink) in planned.downlinks.iter().enumerate() {
                    // The LoRaWAN arm's rule (#96): an explicit `--seq` re-requests pages of an
                    // incomplete answer, every other frame gets a fresh one, and both record
                    // which command the seq was sent as, so the answer can be told apart.
                    let seq = match planned.seq {
                        Some(seq) if planned.downlinks.len() == 1 => {
                            cache.record_sent(dev_eui.as_str(), seq, downlink.tag())?;
                            seq
                        }
                        _ => cache.allocate_seq(dev_eui.as_str(), downlink.tag())?,
                    };
                    let data = downlink.encode(seq, transport)?;
                    if data.len() > crate::p2p::tower::MAX_COMMAND_BODY {
                        return Err(too_long(data.len()));
                    }
                    depth = db
                        .enqueue_downlink(
                            &dev_eui,
                            crate::p2p::session::QueuedDownlink::with_seq(seq, data),
                        )
                        .ok_or_else(no_session)?;
                    if supersede {
                        cache.note_superseding(dev_eui.as_str(), seq)?;
                    }
                    let mut row = serde_json::Map::new();
                    if let Some(name) = planned.keys.get(i) {
                        row.insert("key".to_string(), name.clone().into());
                    }
                    row.insert("seq".to_string(), seq.into());
                    sent.push(serde_json::Value::Object(row));
                }
                db.save(&store)?;
                if let Some(by) = sent.first().and_then(|r| r["seq"].as_u64()) {
                    note_coalesced(
                        &cache,
                        publisher,
                        dev_eui.as_str(),
                        (!serial.is_empty()).then_some(serial.as_str()),
                        &withdraw,
                        by as u32,
                    )
                    .await;
                }

                let mut result = result
                    .with("type", planned.label)
                    .with("fport", command_encode::COMMAND_FPORT)
                    .with("queue_depth", depth as u64)
                    .with("downlinks", sent.clone());
                // The single-frame forms keep the raw form's scalar shape, so a script reading
                // `seq` does not have to special-case which `<type>` it sent.
                if let [only] = sent.as_slice() {
                    result = result.with("seq", only["seq"].clone());
                }
                // Additive: only a send that withdrew something names it.
                if !withdraw.is_empty() {
                    result = result.with("coalesced", withdraw);
                }
                if !planned.keys.is_empty() {
                    result = result.with("keys", planned.keys);
                }
                return Ok(Outcome::Json(
                    serde_json::to_value(result).map_err(json_err)?,
                ));
            }
            // Both forms build their bytes *before* anything is resolved or queued: the wrong
            // payload on the air costs an RX window and a duty-cycle slot, and on a Class-A
            // link the operator finds out minutes later, if at all.
            let planned = match &args.what {
                Some(kind) => Some(command_encode::plan(kind, transport)?),
                None => None,
            };

            let node = node_ref(&args.ident, transport, "node-send")?;
            // `NodeRef::resolve` keeps §5's asymmetric corner: a `--dev-eui` with no
            // enrollment record still addresses ChirpStack directly.
            let nodes = NodesFile::load(&store)?;
            let dev_eui = node.resolve(&nodes)?;

            // A `set-config` packs its keys by the Node's own downlink budget, which only the
            // resolved Node's last uplink tells (its data rate). Planned above at DR0 so bad
            // arguments still fail before anything is resolved.
            let cache = Cache::new(&store, transport);
            let doc = cache.read(dev_eui.as_str())?;
            let dr = doc
                .as_ref()
                .and_then(|d| d.slot(CacheSlot::Message))
                .and_then(|e| e.value.get("dr").and_then(serde_json::Value::as_u64));
            let mut budget = command_encode::carrier_downlink_budget(transport, dr);
            // #166: the network server discarded a frame to this Node as too big for the data
            // rate it actually transmits at; pack into what it said it carries.
            if let Some(cap) = doc
                .as_ref()
                .and_then(|d| d.payload_cap(crate::lorawan::gateway::now_unix()))
            {
                budget = budget.min(cap.max(command_encode::DR0_BUDGET));
            }
            let planned = match (&args.what, planned) {
                (Some(kind @ NodeSendKind::SetConfig { .. }), Some(_)) => {
                    Some(command_encode::plan_with_budget(kind, transport, budget)?)
                }
                (_, planned) => planned,
            };
            // Every other form is one frame of whatever size it came to; one over the budget
            // would be accepted by ChirpStack and silently discarded at transmit (#129).
            if let Some(planned) = &planned {
                command_encode::check_budget(planned, transport, budget, dr)?;
            }

            let c = need_client()?;
            // §5's envelope: the addressed identifier, the type, and `queued: true` — every
            // `node-send` confirms *acceptance*, never completion.
            let mut result = CommandResult::new("node-send", Transport::Lorawan);
            if let NodeRef::Serial(s) = &node {
                result = result.with("serial_number", s.clone());
            }
            result = result
                .with("dev_eui", dev_eui.as_str())
                .with("queued", true);

            let Some(planned) = planned else {
                let (f_port, data) = raw_downlink(args, transport)?;
                command_encode::check_raw_budget(data.len(), transport, budget, dr)?;
                // Unconfirmed: §5 exposes no `--confirmed` flag, and a confirmed downlink is a
                // different contract (it occupies the queue until the device acknowledges or
                // the retries run out), not a default to pick silently.
                let downlink_id = c.enqueue_downlink(&dev_eui, f_port, data, false).await?;
                return Ok(Outcome::Json(
                    serde_json::to_value(
                        result
                            .with("type", "hex")
                            .with("fport", f_port)
                            .with("downlink_id", downlink_id),
                    )
                    .map_err(json_err)?,
                ));
            };

            // The structured forms are fPort 85 by protocol (`--fport` is not offered for
            // them) and unconfirmed for the same reason the raw form is: §9.7's own
            // seq-correlated `Response` is the application-level confirmation, so a
            // LoRaWAN-layer confirmed downlink would duplicate it and spend duty cycle.
            let mut sent = Vec::new();
            let coalescing = coalescing_kind(&planned, transport)?;
            let supersede = superseding(args);
            let mut coalesced = Vec::new();

            for (i, downlink) in planned.downlinks.iter().enumerate() {
                // Persistent per device (§9.7's correlation rule): a CLI process exits, so the
                // high-water mark lives in the node's cache document.
                // `get-config --seq` re-requests pages of an incomplete answer under its own
                // `seq`, so they complete it (#96); every other frame gets a fresh one. Both are
                // recorded, so the answer is known to be a GetConfig's whichever way it went out.
                let seq = match planned.seq {
                    Some(seq) if planned.downlinks.len() == 1 => {
                        cache.record_sent(dev_eui.as_str(), seq, downlink.tag())?;
                        seq
                    }
                    _ => cache.allocate_seq(dev_eui.as_str(), downlink.tag())?,
                };
                let data = downlink.encode(seq, transport)?;
                // Q-N1 (plan.md §5, amended 2026-09-28): an identical idempotent ask still
                // waiting in ChirpStack's queue is withdrawn for this one, from the queue as it
                // stands now. Nothing is withdrawn while an item is on the air (`is_pending`);
                // this frame then queues behind, as before.
                if let Some(kind) = coalescing {
                    let sent_doc = cache.read(dev_eui.as_str())?;
                    coalesced =
                        crate::lorawan::queue::withdraw_waiting(c, &dev_eui, |seq, frame| {
                            crate::coalesce::replaces(kind, seq, frame, sent_doc.as_ref())
                        })
                        .await?;
                } else if supersede && i == 0 {
                    // Q20b (config_sync.md §13): before the first frame of a Node's whole
                    // pending configuration, an earlier one still waiting goes. Only then: the
                    // later frames are this write's own.
                    let sent_doc = cache.read(dev_eui.as_str())?;
                    coalesced =
                        crate::lorawan::queue::withdraw_waiting(c, &dev_eui, |seq, frame| {
                            crate::coalesce::replaces_config(seq, frame, sent_doc.as_ref())
                        })
                        .await?;
                }
                let size = data.len();
                let downlink_id = c
                    .enqueue_downlink(&dev_eui, command_encode::COMMAND_FPORT, data, false)
                    .await
                    .map_err(|e| {
                        if !coalesced.is_empty() {
                            // Withdrawn and not replaced: the operator has to send it again.
                            after_chirpstack(
                                e,
                                &format!(
                                    "the waiting frame(s) seq {coalesced:?} were withdrawn to \
                                     coalesce into this one"
                                ),
                            )
                        } else if i == 0 {
                            e
                        } else {
                            // A partially-queued multi-key write is not a clean failure: the
                            // frames already accepted will still be applied, and the operator
                            // has to know which.
                            after_chirpstack(
                                e,
                                &format!(
                                    "{i} of {} frames were already queued",
                                    planned.downlinks.len()
                                ),
                            )
                        }
                    })?;
                eprintln!("[node-send] {dev_eui}: seq {seq} fport 85 {size} B as {downlink_id}");
                // #129: ChirpStack names the queue item, not the seq, if it discards the frame
                // at transmit time; the frame is queued, so a failed note only costs that link.
                if let Err(e) = cache.note_downlink_id(dev_eui.as_str(), seq, &downlink_id) {
                    eprintln!("[node-send] {dev_eui}: cannot record downlink_id of seq {seq}: {e}");
                }
                if supersede {
                    if let Err(e) = cache.note_superseding(dev_eui.as_str(), seq) {
                        eprintln!("[node-send] {dev_eui}: cannot mark seq {seq} superseding: {e}");
                    }
                }
                let mut row = serde_json::Map::new();
                if let Some(names) = planned.keys.get(i) {
                    // `key` stays for a one-key frame, the shape scripts read before packing.
                    if let [only] = names.as_slice() {
                        row.insert("key".to_string(), only.clone().into());
                    }
                    row.insert("keys".to_string(), names.clone().into());
                }
                row.insert("seq".to_string(), seq.into());
                row.insert("downlink_id".to_string(), downlink_id.into());
                sent.push(serde_json::Value::Object(row));
            }

            if let Some(by) = sent.first().and_then(|r| r["seq"].as_u64()) {
                let serial = nodes.by_dev_eui(&dev_eui).map(|r| r.serial_number.as_str());
                note_coalesced(
                    &cache,
                    publisher,
                    dev_eui.as_str(),
                    serial,
                    &coalesced,
                    by as u32,
                )
                .await;
            }

            let mut result = result
                .with("type", planned.label)
                .with("fport", command_encode::COMMAND_FPORT)
                .with("downlinks", sent.clone());
            // The single-frame forms keep the raw form's scalar shape, so a script that reads
            // `downlink_id` does not have to special-case which `<type>` it sent.
            if let [only] = sent.as_slice() {
                result = result
                    .with("seq", only["seq"].clone())
                    .with("downlink_id", only["downlink_id"].clone());
            }
            // Additive: only a send that withdrew something names it.
            if !coalesced.is_empty() {
                result = result.with("coalesced", coalesced);
            }
            if !planned.keys.is_empty() {
                // Flat at the top, as before packing; per frame in each `downlinks` row.
                let keys: Vec<String> = planned.keys.into_iter().flatten().collect();
                result = result.with("keys", keys);
            }
            Ok(Outcome::Json(
                serde_json::to_value(result).map_err(json_err)?,
            ))
        }

        Command::NodeGet(args) => {
            // P2P is addressed through its own registry (`p2p.md` §7) and its cache, like
            // LoRaWAN's, is keyed by dev_eui (#73), so both identifiers resolve there first:
            // a `--serial` becomes the dev_eui the document is named after, and an identity
            // nobody registered stays a bookkeeping miss rather than an empty answer.
            let p2p_dev_eui = match transport {
                Transport::P2p => {
                    let db = crate::p2p::db::SessionsDb::load(&store)?;
                    Some(resolve_p2p_node(&db, &args.ident, "node-get")?)
                }
                Transport::Lorawan => None,
            };

            // §5: a read of cached, radio-reported data. **Never a downlink** — that is
            // `node-send get-config`/`get-info`, whose async answer this later returns.
            //
            // The liveness gate comes first, before the node is even resolved: with
            // `proximos-control` stopped nothing is filling the cache, and §5 requires
            // `DaemonDown` for the cache-backed reads rather than a silent stale answer. A
            // node that is genuinely absent would otherwise be reported `NotFound` on a Hub
            // whose real problem is that the daemon is not running.
            //
            // **Both carriers** since PI1 (`plan/portal/p2p_integration.md` §4.1): the P2P
            // central is a task inside `proximos-control`, and that process touches the
            // heartbeat only while every enabled radio task is alive — so a stale file means
            // nothing is filling this cache, whichever arm is asked.
            if !crate::tasks::heartbeat::is_fresh(crate::tasks::health::interval()) {
                return Err(crate::tasks::heartbeat::daemon_down("node-get"));
            }

            let node = node_ref(&args.ident, transport, "node-get")?;
            // The cache is keyed by the node's wire identity, which is the DevEUI on both
            // carriers since #73 — already resolved above on P2P — so a LoRaWAN `--serial`
            // has to be resolved locally first. §5's asymmetric corner survives that: a
            // `--dev-eui` with no enrollment record still addresses the cache.
            let id = match p2p_dev_eui {
                Some(dev_eui) => dev_eui,
                None => node
                    .resolve(&NodesFile::load(&store)?)?
                    .as_str()
                    .to_string(),
            };

            let slot = CacheSlot::from(args.what);
            let cache = Cache::new(&store, transport);
            let doc = cache.read(&id)?.ok_or_else(|| {
                Error::new(
                    Kind::NotFound,
                    transport,
                    Component::EnrollmentRecord,
                    format!(
                        "nothing has been received from {} yet: node-get reads what the device \
                         reported over radio, and no uplink from it has been decoded",
                        describe(&node)
                    ),
                )
            })?;
            let entry = doc.slot(slot).ok_or_else(|| {
                Error::new(
                    Kind::NotFound,
                    transport,
                    Component::EnrollmentRecord,
                    format!(
                        "{} has been heard from, but never with {:?} data; filled so far: [{}]. \
                         `node-get` never triggers a downlink — use `node-send get-config`/\
                         `get-info` and read it back once the device answers",
                        describe(&node),
                        slot.as_str(),
                        doc.filled()
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
            })?;

            // The value plus when it arrived. The timestamp is not decoration: this is data
            // that may be hours old, and a consumer that cannot see how old would present a
            // stale config dump as the device's current state.
            Ok(Outcome::Json(serde_json::json!({
                "command": "node-get",
                "transport": transport.as_str(),
                "id": id,
                "type": slot.as_str(),
                "received_at": entry.at,
                "value": entry.value,
            })))
        }

        Command::RadioSet(args) => {
            if transport == Transport::Lorawan {
                // Unreachable through the mode gate, which refuses the unimplemented arm of a
                // one-armed command first (`mode::resolve`). Kept because it is the arm's own
                // reason, and a `match` on the resolved transport has to answer for it anyway.
                return Err(Error::new(
                    Kind::InvalidArgument,
                    Transport::Lorawan,
                    Component::ControlSocket,
                    "radio-set has no LoRaWAN arm: the data rate is ADR's on LoRaWAN, \
                     not an operator setting",
                ));
            }
            let config_sf = config
                .p2p_radio
                .as_ref()
                .ok_or_else(|| {
                    Error::new(
                        Kind::InvalidArgument,
                        Transport::P2p,
                        Component::RadioMode,
                        "radio.p2p_radio is absent — there are no radio parameters to override",
                    )
                })?
                .spreading_factor;

            // Load-mutate-save under the session-DB lock, the same critical section `node-send`
            // uses: the running central writes this file on every join and every uplink, and an
            // unlocked read-modify-write would lose one of the two updates.
            let _db_lock = store.lock(Slot::P2pSessions)?;
            let mut db = crate::p2p::db::SessionsDb::load(&store)?;
            let generation = match args.spreading_factor {
                // Bumped on every set, including a set to the value already recorded: the
                // operator asked for it to be pushed, and the central's whole trigger is the
                // generation moving.
                Some(sf) => {
                    let next = db.radio.as_ref().map_or(0, |r| r.generation) + 1;
                    db.radio = Some(crate::p2p::db::RadioOverride {
                        spreading_factor: sf,
                        generation: next,
                        set_at: gateway::format_rfc3339(gateway::now_unix()),
                    });
                    next
                }
                // `--reset`: the record goes, and with it the generation. The central sees a
                // generation it has not applied and switches back to the config key.
                None => {
                    db.radio = None;
                    0
                }
            };
            db.save(&store)?;
            drop(_db_lock);

            let effective = args.spreading_factor.unwrap_or(config_sf);
            Ok(Outcome::Json(
                serde_json::to_value(
                    CommandResult::new("radio-set", Transport::P2p)
                        .with(
                            "spreading_factor",
                            args.spreading_factor
                                .map_or(serde_json::Value::Null, |sf| sf.into()),
                        )
                        .with("config_spreading_factor", config_sf)
                        .with("effective_spreading_factor", effective)
                        .with("generation", generation),
                )
                .map_err(json_err)?,
            ))
        }

        Command::NorthbridgeUpdate(args) => {
            // Mode-independent (#158): the P2P task or, without P2P, the idle Northbridge
            // task answers; `request_update` refuses when neither runs.
            let req = crate::p2p::nb_update::CliRequest {
                hotfix: args.file.clone().zip(args.manifest.clone()),
                force: args.force,
                wait: (!args.no_wait).then(|| std::time::Duration::from_secs(args.timeout)),
            };
            let task = read_run_document("northbridge.json");
            let out = crate::p2p::nb_update::request_update(
                &store,
                &req,
                &crate::p2p::nb_update::bundle_dir(),
                task.as_ref(),
                std::time::Duration::from_millis(500),
            )
            .await?;
            let mut result = CommandResult::new("northbridge-update", Transport::P2p)
                .with("generation", out.request.generation)
                .with("source", out.request.source.as_str())
                .with("image", out.image)
                .with("force", out.request.force)
                .with("done", out.result.is_some());
            if let Some(r) = out.result {
                result = result.with("message", r.message);
            }
            Ok(Outcome::Json(
                serde_json::to_value(result).map_err(json_err)?,
            ))
        }

        // What is left is the P2P scan pair, which needs the session loop.
        _ => Err(phase2(&format!(
            "`{}` needs state the agent owns; it lands when control-radio's loop duties become \
             tasks in proximos-control (decision #12)",
            cli.command.name()
        ))),
    }
}

/// `node-add --radio lorawan` once the arguments are parsed and the key material is in hand:
/// enrol the device in ChirpStack, record it in `nodes.json`, publish its retained `info`
/// document.
///
/// Shared by the CLI arm in [`run_with_store`] and by the agent's signed `node-add` MQTT
/// command (`control-core`'s `authorization::manager`), which takes the key from the signed
/// params instead of `--keys-file`/`--keys-stdin`. One function, so the two entry points
/// cannot disagree on what "enrolled" means — v1 had two enrolment paths writing the same
/// ChirpStack objects, and `control-core`'s radio gate exists because of it.
///
/// `join_eui: None` means `radio.join_eui` from the config: `--join-eui` *overrides* the
/// configured default (plan.md §5), so the default has to apply when the caller passes
/// nothing, or the device lands in ChirpStack with an all-zero JoinEUI.
pub async fn enroll_lorawan_node(
    mut req: EnrollRequest<'_>,
    config: &RadioConfig,
    client: &dyn ChirpStack,
    publisher: &dyn RetainedPublisher,
    store: &Store,
) -> Result<CommandResult> {
    let tokens = Tokens::new(store.clone());
    let conv = Converger {
        client,
        config,
        tokens: &tokens,
        store,
    };
    // Reads the state `create` already converged — enrolling must never itself re-converge,
    // that would blur two commands into one.
    let state = conv.load_state()?;
    let enroller = Enroller {
        client,
        store,
        state: &state,
    };
    if req.join_eui.is_none() {
        req.join_eui = config.join_eui.clone();
    }
    let outcome = enroller.enroll(req).await?;

    // §9.4: the per-node `info` topic is retained, and it is where a late subscriber —
    // `portal` included — learns the inventory from. Published by the command rather than by
    // the daemon because an enrollment invisible until the next daemon tick is
    // indistinguishable, from the outside, from one that failed.
    //
    // The document is re-read from `nodes.json` instead of assembled from `EnrollOutcome`:
    // that file is what `node-info` renders, so the retained document is provably the same
    // record minus the key rather than a parallel shape that can drift from it.
    //
    // Found by DevEUI, never by `outcome.serial_number`: the record is keyed by the canonical
    // 8-hex serial (node_registry.md §2) while the app sends the decimal one, so a lookup by the
    // typed serial missed and the retained document was `null` — a Node invisible in the
    // portal until its join (#157). A miss is an error, never a `null` publish.
    let stored = NodesFile::load(store)?;
    let info = stored
        .by_serial
        .values()
        .find(|r| r.dev_eui == outcome.dev_eui)
        .map(|r| r.to_json(KeyDisclosure::Omitted))
        .ok_or_else(|| {
            Error::new(
                Kind::StorageError,
                Transport::Lorawan,
                Component::EnrollmentRecord,
                format!(
                    "the node IS enrolled in ChirpStack, but no enrollment record names {}; \
                     its retained info document was not published",
                    outcome.dev_eui.as_str()
                ),
            )
        })?;
    let topic = Topics::new(northbound::hostname()).node(outcome.dev_eui.as_str(), Area::Info);
    let payload = serde_json::to_vec(&info).map_err(json_err)?;
    publisher
        .publish_retained(&[(topic.clone(), payload)])
        .await
        .map_err(|e| {
            after_chirpstack(
                e,
                &format!(
                    "the node IS enrolled in ChirpStack, but its retained {topic} document \
                     was not published, so the inventory does not show it yet; re-run \
                     node-add to publish it"
                ),
            )
        })?;

    Ok(CommandResult::new("node-add", Transport::Lorawan)
        .with("serial_number", outcome.serial_number)
        .with("dev_eui", outcome.dev_eui.as_str())
        .with("profile", outcome.profile)
        .with("activation", outcome.activation))
}

/// `node-remove --radio lorawan`, the counterpart of [`enroll_lorawan_node`] and shared with
/// the agent's signed `node-remove` the same way.
///
/// Three steps, in this order, and the order is the whole design (lorawan.md open q13):
/// delete the device, clear the retained topics, *then* drop the local record. `--serial`
/// only resolves while the record is there, so dropping it before the tombstone is published
/// would make a broker failure unrecoverable by the identifier the operator typed — the
/// re-run would answer `NotFound` and the node would stay in every subscriber's inventory
/// forever.
///
/// `dev_eui` is `node` already resolved (`NodeRef::resolve`) — the caller does that before it
/// opens a ChirpStack or broker connection, so a `--serial` nobody enrolled fails as the
/// bookkeeping miss it is, not as a connection problem.
pub async fn remove_lorawan_node(
    node: NodeRef,
    dev_eui: Eui,
    config: &RadioConfig,
    client: &dyn ChirpStack,
    publisher: &dyn RetainedPublisher,
    store: &Store,
) -> Result<CommandResult> {
    let tokens = Tokens::new(store.clone());
    let conv = Converger {
        client,
        config,
        tokens: &tokens,
        store,
    };
    let state = conv.load_state()?;
    let enroller = Enroller {
        client,
        store,
        state: &state,
    };

    enroller.delete_device(&dev_eui).await?;

    let stones: Vec<(String, Vec<u8>)> = Topics::new(northbound::hostname())
        .tombstones(dev_eui.as_str())
        .into_iter()
        .map(|t| (t, northbound::TOMBSTONE.to_vec()))
        .collect();
    let names = stones
        .iter()
        .map(|(t, _)| t.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    publisher.publish_retained(&stones).await.map_err(|e| {
        after_chirpstack(
            e,
            &format!(
                "the device IS deleted in ChirpStack, but the retained {names} still holds \
                 its info document; re-run node-remove to clear it"
            ),
        )
    })?;

    // The cached radio-reported data goes with the record. Between the tombstone and
    // `forget`, so the same re-runnability argument holds: while the record is still there
    // the whole command can be repeated by `--serial`.
    Cache::new(store, Transport::Lorawan).remove(dev_eui.as_str())?;
    // A P2P session the Node parked when it moved to LoRaWAN goes with it (node_registry.md §4,
    // amended 2026-09-28): `node-remove` drops the session, parked or live.
    crate::p2p::db::drop_parked_or_log(store, &[dev_eui.as_str().to_string()]);

    enroller.forget(&node, &dev_eui)?;

    // The addressed identifier per plan.md §5, plus `dev_eui` always — it is the wire identity
    // and the caller may only have held a serial.
    let mut result = CommandResult::new("node-remove", Transport::Lorawan);
    if let NodeRef::Serial(s) = &node {
        result = result.with("serial_number", s.clone());
    }
    Ok(result
        .with("dev_eui", dev_eui.as_str())
        .with("state", "deleted"))
}

/// The forwarder's topic prefix. v1 sets it to the region name, and the forwarder config is
/// rendered from the same input, so the region is the right default until the renderer exposes
/// the key explicitly.
///
/// Public because the Server's broker ACL names it too: a Client Hub's forwarder publishes
/// under `<prefix>/gateway/<eui>/#` on the Server's 8883 listener, and the account that lets it
/// is written by control-core (`plan/control/cluster.md` §5, §7). One function, so the ACL and
/// the topics ChirpStack subscribes to cannot name two prefixes.
pub fn forwarder_topic_prefix(config: &RadioConfig) -> String {
    std::env::var("PROXIMOS_FORWARDER_TOPIC_PREFIX")
        .unwrap_or_else(|_| config.region.as_str().to_string())
}

/// One arm of `create`: the LoRaWAN converge with its ChirpStack mirror, or the P2P init.
async fn create_arm(
    transport: Transport,
    args: &crate::cli::CreateArgs,
    config: &RadioConfig,
    client: Option<&dyn ChirpStack>,
    topics: Option<&dyn gateway::RetainedTopics>,
    tokens: &Tokens,
    store: &Store,
) -> Result<serde_json::Value> {
    match transport {
        Transport::Lorawan => {
            let eui = match &args.gateway_eui {
                Some(e) => Some(Eui::parse(e)?),
                None => match topics {
                    Some(t) => {
                        let prefix = forwarder_topic_prefix(config);
                        let found = gateway::discover_all(t, &prefix, DISCOVERY_TIMEOUT).await?;
                        // #169: a cluster Client's forwarder (or a swapped card) leaves its
                        // retained state on this broker too. The local concentrator is the one
                        // registered under the local gateway name; a Client Hub's gateway is
                        // registered under its hostname.
                        let mut known = Vec::new();
                        if found.len() > 1 {
                            if let Some(c) = client {
                                for g in &found {
                                    if let Ok(Some(gw)) = c.get_gateway(g).await {
                                        if gw.name == config.gateway.name {
                                            known.push(g.clone());
                                        }
                                    }
                                }
                            }
                        }
                        gateway::pick(found, &known, &gateway::state_topic_filter(&prefix))?
                    }
                    None => None,
                },
            };
            let c = client.ok_or_else(|| {
                Error::new(
                    Kind::ServiceDown,
                    Transport::Lorawan,
                    Component::ChirpstackService,
                    "no ChirpStack connection: this command needs the LoRaWAN arm".to_string(),
                )
            })?;
            let conv = Converger {
                client: c,
                config,
                tokens,
                store,
            };
            let report = conv.create(eui.as_ref(), args.dry_run).await?;
            let mut json = serde_json::to_value(report).map_err(json_err)?;
            // ChirpStack follows the Node registry (node_registry.md §3). A failed pass
            // is reported, not fatal: the boot must still come up, and the next
            // `create` repairs it.
            if !args.dry_run {
                let mirror = match conv.load_state() {
                    Ok(state) => crate::lorawan::mirror::reconcile(c, store, &state, false).await,
                    Err(e) => Err(e),
                };
                if let Some(obj) = json.as_object_mut() {
                    match mirror {
                        Ok(m) => {
                            obj.insert("mirror".into(), serde_json::to_value(m).map_err(json_err)?);
                        }
                        Err(e) => {
                            eprintln!("[registry] chirpstack mirror failed: {e}");
                            obj.insert("mirror_error".into(), e.message().into());
                        }
                    }
                }
            }
            Ok(json)
        }
        Transport::P2p => {
            let report = create_p2p(store, args.dry_run)?;
            serde_json::to_value(report).map_err(json_err)
        }
    }
}

fn create_p2p(store: &Store, dry_run: bool) -> Result<P2pReport> {
    let existing = store.read(Slot::P2pNetId)?;
    let net_id = match existing {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            let generated = crate::p2p::session::NetId::generate()?.to_hex();
            if !dry_run {
                store.write(Slot::P2pNetId, &generated)?;
            }
            generated
        }
    };
    Ok(P2pReport {
        initialized: true,
        provisioned: store.is_provisioned(Transport::P2p),
        net_id: Some(net_id),
        session_db: if store.exists(Slot::P2pSessions) {
            "ok"
        } else {
            "absent"
        },
        // `create` provisions; it does not report what the radio is running. The key is skipped
        // when absent, so this envelope is byte-identical to what it was — `status` is where the
        // effective spreading factor is answered.
        spreading_factor: None,
        northbridge: None,
    })
}

/// §5's P2P registration bundle: `--serial` + `--dev-eui` + `--keys-file`/`--keys-stdin`, all
/// three required, plus a parseable EUI.
///
/// A Node is registered on exactly one carrier (decision #23, amended 2026-09-27): `node-add` on
/// one refuses a DevEUI the other's registry holds, `AlreadyExists` on the node registry, naming
/// the carrier to `node-remove` it from first. Changing a Node's carrier is a move: remove, then
/// add. The P2P registry is `sessions.db`'s nodes, the LoRaWAN one the enrollment records in
/// `nodes.json`.
///
/// Since the one Node registry (node_registry.md) this holds on every mode: `node-move` changes a
/// Node's carrier whatever the mode, so the refusal is never a dead end.
fn refuse_if_on_the_other_carrier(
    _config: &RadioConfig,
    store: &Store,
    dev_eui: &Eui,
    transport: Transport,
) -> Result<()> {
    let other = match transport {
        Transport::Lorawan => Transport::P2p,
        Transport::P2p => Transport::Lorawan,
    };
    let held = match other {
        Transport::P2p => crate::p2p::db::SessionsDb::load(store)?
            .node(dev_eui.as_str())
            .is_some(),
        Transport::Lorawan => NodesFile::load(store)?.by_dev_eui(dev_eui).is_some(),
    };
    if !held {
        return Ok(());
    }
    Err(Error::new(
        Kind::AlreadyExists,
        transport,
        Component::NodeRegistry,
        format!(
            "dev_eui {} is already registered on {other}; move it with node-move --to \
             {transport} (a Node is registered on one carrier only)",
            dev_eui.as_str()
        ),
    ))
}

/// Unlike LoRaWAN there is no `--otaa`/`--abp` axis and no ChirpStack round trip, so the whole
/// P2P validation is local — which is why it can be enforced before the node registry exists.
/// Validate the P2P registration bundle and hand back the **canonical** DevEUI.
///
/// Returning it rather than `()` is the point: `Eui::parse` accepts separators and any letter
/// case (it is what is printed on a device label), and the DevEUI is the registry key since
/// #73. Storing the operator's raw spelling as that key breaks nothing visibly — every
/// `--serial` command keeps working — while the node's JoinRequest, which carries the canonical
/// bytes, never matches and is dropped forever.
fn p2p_registration_bundle(args: &NodeAddArgs, transport: Transport) -> Result<Eui> {
    let missing = |what: &str| {
        Error::new(
            Kind::InvalidArgument,
            transport,
            Component::ControlSocket,
            format!(
                "node-add --radio p2p needs the whole registration bundle: {what} is missing \
                 (--serial, --dev-eui and --keys-file/--keys-stdin are all required)"
            ),
        )
    };
    if args.serial.is_none() {
        return Err(missing("--serial"));
    }
    let dev_eui = args
        .dev_eui
        .as_deref()
        .ok_or_else(|| missing("--dev-eui"))?;
    // `Eui::parse` is LoRaWAN-side code and stamps `Transport::Lorawan` on its refusal. §6 is
    // explicit that an error never names a transport the caller did not mention, so re-tag it
    // rather than reporting a P2P typo as a LoRaWAN failure. The component stays
    // `enrollment-record`, which is what the LoRaWAN arm reports for the identical bad input —
    // one malformed EUI should not diagnose differently depending on the transport.
    let dev_eui = Eui::parse(dev_eui).map_err(|mut e| {
        e.0.transport = Some(transport);
        e
    })?;
    if args.keys_file.is_none() && !args.keys_stdin {
        return Err(missing("--keys-file/--keys-stdin"));
    }
    Ok(dev_eui)
}

/// `--keys-file <path>` or `--keys-stdin` — never argv (ledger #22). clap's `conflicts_with`
/// on the two flags already rules out both at once; this handles neither and parses whichever
/// one was given.
fn read_key_material(args: &NodeAddArgs, transport: Transport) -> Result<KeyMaterial> {
    let contents = if let Some(path) = &args.keys_file {
        std::fs::read_to_string(path).map_err(|e| {
            Error::new(
                Kind::InvalidArgument,
                transport,
                Component::EnrollmentRecord,
                format!("cannot read key material from {}: {e}", path.display()),
            )
        })?
    } else if args.keys_stdin {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).map_err(|e| {
            Error::new(
                Kind::InvalidArgument,
                transport,
                Component::EnrollmentRecord,
                format!("cannot read key material from stdin: {e}"),
            )
        })?;
        buf
    } else {
        return Err(Error::new(
            Kind::InvalidArgument,
            transport,
            Component::EnrollmentRecord,
            "node-add needs key material: --keys-file <path> or --keys-stdin",
        ));
    };
    keyfile::parse(&contents)
}

/// Resolve a `--serial`/`--dev-eui` selector to the registry's key: the canonical `dev_eui`
/// (#73).
///
/// The one resolver every P2P arm goes through. `--dev-eui` is the canonical spelling and a
/// direct map hit; `--serial` stays as an operator-facing alias — it is what is printed on the
/// device — and costs a registry scan. Either way an unregistered node is `NotFound` against
/// `node-registry` before anything else happens, so the diagnosis is about the identifier the
/// operator actually typed.
fn resolve_p2p_node(
    db: &crate::p2p::db::SessionsDb,
    ident: &crate::cli::NodeIdent,
    command: &str,
) -> Result<String> {
    let transport = Transport::P2p;
    match (ident.serial.as_deref(), ident.dev_eui.as_deref()) {
        (Some(s), None) => {
            let serial = crate::p2p::session::canonical_serial(s).ok_or_else(|| {
                Error::new(
                    Kind::InvalidArgument,
                    transport,
                    Component::ControlSocket,
                    format!(
                        "{command} --serial must be the device's numeric serial (decimal, \
                         0x-hex, or 8 hex digits)"
                    ),
                )
            })?;
            db.node_by_serial(&serial)
                .map(|n| n.dev_eui.clone())
                .ok_or_else(|| {
                    Error::new(
                        Kind::NotFound,
                        transport,
                        Component::NodeRegistry,
                        format!("no P2P node registered with serial {serial}"),
                    )
                })
        }
        // Re-tagged onto P2P: `Eui::parse` reports against LoRaWAN's `EnrollmentRecord`, and a
        // P2P command's errors belong to P2P (§6) — the same re-tag `p2p_registration_bundle`
        // already does.
        (None, Some(e)) => {
            let eui = crate::lorawan::Eui::parse(e).map_err(|mut err| {
                err.0.transport = Some(transport);
                err
            })?;
            if db.node(eui.as_str()).is_none() {
                return Err(Error::new(
                    Kind::NotFound,
                    transport,
                    Component::NodeRegistry,
                    format!("no P2P node registered with dev_eui {}", eui.as_str()),
                ));
            }
            Ok(eui.as_str().to_string())
        }
        _ => Err(Error::new(
            Kind::InvalidArgument,
            transport,
            Component::ControlSocket,
            format!("{command} needs exactly one of --serial or --dev-eui"),
        )),
    }
}

/// Lowercase hex, for persisting a key as the `p2p.md` §7 tables spell it.
fn hex_lower(b: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// `--serial` or `--dev-eui`, as a [`NodeRef`].
///
/// clap already makes the group required and single-valued, so the third arm is defence in
/// depth for a direct library caller — and it produces the same §6 `InvalidArgument` (exit 2)
/// a malformed argv would.
fn node_ref(ident: &NodeIdent, transport: Transport, command: &str) -> Result<NodeRef> {
    match (ident.serial.as_deref(), ident.dev_eui.as_deref()) {
        (Some(s), None) => Ok(NodeRef::Serial(s.to_string())),
        (None, Some(e)) => Ok(NodeRef::DevEui(Eui::parse(e)?)),
        _ => Err(Error::new(
            Kind::InvalidArgument,
            transport,
            Component::ControlSocket,
            format!("{command} needs exactly one of --serial or --dev-eui"),
        )),
    }
}

/// How a message should name the node the caller addressed.
fn describe(node: &NodeRef) -> String {
    match node {
        NodeRef::Serial(s) => format!("serial {s:?}"),
        NodeRef::DevEui(e) => format!("dev_eui {e}"),
    }
}

/// One `node-list` element: node-info's common core, trimmed of key material, joined with what
/// ChirpStack knows.
fn node_list_row(
    device: &crate::lorawan::model::Device,
    record: Option<&EnrollmentRecord>,
    state: &ConvergedState,
) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    if let Some(r) = record {
        m.insert("serial_number".into(), r.serial_number.clone().into());
    }
    m.insert("dev_eui".into(), device.dev_eui.as_str().into());
    m.insert("transport".into(), Transport::Lorawan.as_str().into());

    // The record's own `--profile` value wins; for a device enrolled out of band the converged
    // id map still names the profile, which is better than reporting none.
    let by_id = profile_of(state, &device.device_profile_id);
    match (record, &by_id) {
        (Some(r), _) => {
            m.insert("profile".into(), r.profile.clone().into());
            m.insert("activation".into(), r.activation.clone().into());
        }
        (None, Some((name, activation))) => {
            m.insert("profile".into(), (*name).into());
            m.insert("activation".into(), activation.as_str().into());
        }
        (None, None) => {}
    }

    let name = record
        .and_then(|r| r.name.clone())
        .or_else(|| Some(device.name.clone()).filter(|n| !n.is_empty()));
    if let Some(n) = name {
        m.insert("name".into(), n.into());
    }

    // A LoRaWAN device that has been heard from has joined; one ChirpStack has never seen an
    // uplink from has not. There is no `pending` on this arm — that value belongs to the P2P
    // pairing flow. (`node-info` cannot say even this much: it makes no ChirpStack call.)
    m.insert(
        "pairing_status".into(),
        match device.last_seen_at {
            Some(_) => "joined".into(),
            None => "never".into(),
        },
    );
    m.insert(
        "last_seen".into(),
        match &device.last_seen_at {
            Some(s) => s.clone().into(),
            None => serde_json::Value::Null,
        },
    );
    // §5's per-gateway `rssi`/`snr` rows come from the uplink metadata, which only the WP-D
    // cache holds — ChirpStack's device list carries no radio metrics. Null, explicitly, so
    // the key is present and a consumer can tell "not known yet" from "no gateway heard it".
    m.insert("gateways".into(), serde_json::Value::Null);

    serde_json::Value::Object(m)
}

/// Which declared profile a ChirpStack `device_profile_id` belongs to.
fn profile_of<'a>(state: &'a ConvergedState, id: &str) -> Option<(&'a str, Activation)> {
    let find = |m: &'a std::collections::BTreeMap<String, String>| {
        m.iter()
            .find(|(_, v)| v.as_str() == id)
            .map(|(k, _)| k.as_str())
    };
    find(&state.device_profile_ids.otaa)
        .map(|n| (n, Activation::Otaa))
        .or_else(|| find(&state.device_profile_ids.abp).map(|n| (n, Activation::Abp)))
}

/// The tenant id for a read-only command.
///
/// `config.json` first: `create` recorded it there, and the read-only token cannot resolve a
/// tenant at all — `TenantService.List` is admin-scoped (token.rs). Falling back to the RPC
/// keeps a Hub whose `config.json` predates the field working instead of telling its operator
/// to re-run `create`.
async fn tenant_for_read(conv: &Converger<'_>) -> Result<String> {
    match conv.load_state()?.tenant_id {
        Some(t) if !t.is_empty() => Ok(t),
        _ => conv.resolve_tenant_id().await,
    }
}

/// `node-send --hex <payload> --fport <n>`.
///
/// The port is not defaulted — see the flag's own note in `cli.rs`. clap enforces the pairing,
/// so the refusal here is for a direct library caller.
/// `node-send dev-status|backoff|rejoin --radio p2p`: queue one `0x91` control command for a
/// joined Node whose Capabilities name it (p2p_control_commands.md §2, H-7).
fn send_p2p_control(
    store: &Store,
    ident: &NodeIdent,
    ctl: crate::p2p::tower::control::ControlDownlink,
) -> Result<Outcome> {
    let transport = Transport::P2p;
    let _db_lock = store.lock(Slot::P2pSessions)?;
    let mut db = crate::p2p::db::SessionsDb::load(store)?;
    let dev_eui = resolve_p2p_node(&db, ident, "node-send")?;
    let serial = db
        .node(&dev_eui)
        .map(|n| n.serial.clone())
        .unwrap_or_default();
    let session = db.sessions.get(&dev_eui).ok_or_else(|| {
        Error::new(
            Kind::NotFound,
            transport,
            Component::SessionDb,
            format!(
                "no live P2P session for {dev_eui} — the node has not joined, so there is no \
                 session key to seal a downlink under"
            ),
        )
    })?;
    crate::p2p::tower::control::gate(session.capabilities.as_ref(), ctl.cmd()).map_err(|why| {
        Error::new(
            Kind::InvalidArgument,
            transport,
            Component::SessionDb,
            format!("node-send {} refused for {dev_eui}: {why}", ctl.name()),
        )
    })?;
    let queued = session.downlink_queue.len();
    if queued >= crate::p2p::db::MAX_QUEUED_DOWNLINKS {
        return Err(Error::new(
            Kind::InvalidArgument,
            transport,
            Component::SessionDb,
            format!(
                "{dev_eui} already has {queued} downlink(s) queued, the {} a node may have \
                 waiting; let the queue drain or node-remove it",
                crate::p2p::db::MAX_QUEUED_DOWNLINKS
            ),
        ));
    }
    let depth = db
        .enqueue_downlink(&dev_eui, crate::p2p::session::QueuedDownlink::control(ctl))
        .unwrap_or(queued + 1);
    db.save(store)?;
    let mut result = CommandResult::new("node-send", transport)
        .with("serial_number", serial)
        .with("dev_eui", dev_eui.as_str())
        .with("queued", true)
        .with("state", "queued")
        .with("type", ctl.name())
        .with("envelope", "0x91")
        .with("queue_depth", depth as u64);
    if let crate::p2p::tower::control::ControlDownlink::Backoff { seconds } = ctl {
        result = result.with("seconds", u64::from(seconds));
    }
    Ok(Outcome::Json(
        serde_json::to_value(result).map_err(json_err)?,
    ))
}

fn raw_downlink(args: &NodeSendArgs, transport: Transport) -> Result<(u8, Vec<u8>)> {
    let usage = |m: String| {
        Error::new(
            Kind::InvalidArgument,
            transport,
            Component::ControlSocket,
            m,
        )
    };
    let hex = args
        .hex
        .as_deref()
        .ok_or_else(|| usage("node-send needs --hex <payload> or a <type>".to_string()))?;
    let f_port = args.fport.ok_or_else(|| {
        usage("node-send --hex needs --fport <u8>: the port is not defaulted".to_string())
    })?;
    let data = parse_hex(hex).map_err(usage)?;
    Ok((f_port, data))
}

/// Strict hex — even length, hex digits only, no separators.
///
/// Deliberately stricter than [`Eui::parse`], which accepts `:`/`-` because an EUI is
/// customarily written that way. A downlink payload is not: a separator in it is far more
/// likely to be a mistyped byte than a formatting choice, and guessing would put the wrong
/// bytes on the air.
fn parse_hex(s: &str) -> std::result::Result<Vec<u8>, String> {
    if s.is_empty() {
        return Err("the --hex payload is empty: there is nothing to send".to_string());
    }
    // Checked before the pairwise parse, not left to it: `u8::from_str_radix` accepts a
    // leading sign, so `"+a"` would otherwise pass as the byte `0x0a`.
    if !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "the --hex payload is not hexadecimal (digits 0-9a-f only, no separators): {s:?}"
        ));
    }
    if !s.len().is_multiple_of(2) {
        return Err(format!(
            "the --hex payload has an odd number of digits ({}), so its last byte is \
             incomplete",
            s.len()
        ));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            s.get(i..i + 2)
                .and_then(|b| u8::from_str_radix(b, 16).ok())
                .ok_or_else(|| format!("the --hex payload is not hexadecimal: {s:?}"))
        })
        .collect()
}

/// What the one frame of `planned` coalesces as (plan.md §5 `node-send`, amended 2026-09-28),
/// or `None`: a write of several frames, a continuation under `--seq`, or a kind that never
/// coalesces. Read from the frame's own bytes by [`crate::coalesce::kind`], the one decision
/// both carriers share. The `seq` does not change it, so the frame is encoded under a stand-in
/// before its own is allocated.
fn coalescing_kind(
    planned: &command_encode::Plan,
    transport: Transport,
) -> Result<Option<crate::decode::command::CommandTag>> {
    let [only] = planned.downlinks.as_slice() else {
        return Ok(None);
    };
    Ok(crate::coalesce::kind(
        &only.encode(1, transport)?,
        planned.seq.is_some(),
    ))
}

/// Whether this `node-send` is a `set-config --supersede` (config_sync.md §13 Q20b).
fn superseding(args: &crate::cli::NodeSendArgs) -> bool {
    matches!(
        args.what,
        Some(NodeSendKind::SetConfig {
            supersede: true,
            ..
        })
    )
}

/// What a coalescing `node-send` owes once its frame is queued under `by`: each withdrawn
/// seq's `sent_commands` entry names `by` (`superseded_by`), and a `status: superseded`
/// `command` document goes out for it when this invocation has a broker, so a waiter closes a
/// request the Node will not answer under its own seq.
///
/// The frame is queued by then, so neither failing fails the command, which would only invite
/// a retry that queues another frame; each failure is reported on stderr.
async fn note_coalesced(
    cache: &Cache,
    publisher: Option<&dyn RetainedPublisher>,
    dev_eui: &str,
    serial: Option<&str>,
    withdrawn: &[u32],
    by: u32,
) {
    if withdrawn.is_empty() {
        return;
    }
    let at = gateway::format_rfc3339(gateway::now_unix());
    let topic = Topics::new(northbound::hostname()).node(dev_eui, Area::Command);
    let mut docs = Vec::new();
    for &seq in withdrawn {
        if let Err(e) = cache.note_superseded(dev_eui, seq, by) {
            eprintln!("[node-send] {dev_eui}: cannot record seq {seq} as superseded: {e}");
        }
        let doc = crate::coalesce::superseded_document(dev_eui, serial, seq, by, &at);
        docs.push((topic.clone(), serde_json::to_vec(&doc).unwrap_or_default()));
    }
    if let Some(broker) = publisher {
        if let Err(e) = broker.publish_events(&docs).await {
            eprintln!(
                "[node-send] {dev_eui}: cannot publish the superseded seqs {withdrawn:?}: {e}"
            );
        }
    }
}

/// Re-tag a broker failure that happened **after** ChirpStack had already been changed.
///
/// The variant and the component are kept — a script branches on those, and both are still
/// true — so only the message changes. It has to, because an operator reading `ServiceDown`
/// from `node-remove` would otherwise conclude the device was not deleted, when it was, and
/// the remedy is a re-run of the very same command.
fn after_chirpstack(e: Error, what: &str) -> Error {
    Error::new(
        e.kind(),
        Transport::Lorawan,
        e.component(),
        format!("{what} ({})", e.message()),
    )
}

/// `create --role client` (plan/control/cluster.md §8, lorawan.md §8.2): the cluster join,
/// handed to the agent over its control socket with the password from stdin. It makes no gRPC
/// call: the join never touches ChirpStack, and after the reboot this Hub runs none.
///
/// The join is the same for either arm (`p2p_cluster_gateway.md` §3.1): after it a P2P Client
/// lends its Northbridge to the Server's central, a LoRaWAN one its concentrator.
fn create_client(transport: Transport, args: &crate::cli::CreateArgs) -> Result<Outcome> {
    if args.dry_run {
        return Err(Error::new(
            Kind::InvalidArgument,
            transport,
            Component::ControlSocket,
            "create --role client has no dry run: the join's checks talk to the Server, and a \
             successful one reboots this Hub"
                .to_string(),
        ));
    }
    let (Some(server_host), Some(ca_sha256)) = (&args.server_host, &args.ca_sha256) else {
        return Err(Error::new(
            Kind::InvalidArgument,
            transport,
            Component::ControlSocket,
            "create --role client needs --server-host and --ca-sha256".to_string(),
        ));
    };
    let mut stdin = std::io::stdin().lock();
    crate::cluster_join::run(
        server_host,
        ca_sha256,
        &mut stdin,
        &crate::cluster_join::socket_path(),
    )
    .map(Outcome::Json)
}

fn phase2(why: &str) -> Error {
    Error::new(
        Kind::Phase2NotImplemented,
        None,
        Component::ControlSocket,
        why.to_string(),
    )
}

fn json_err(e: serde_json::Error) -> Error {
    Error::new(
        Kind::StorageError,
        None,
        Component::ControlSocket,
        format!("could not render the result as JSON: {e}"),
    )
}

/// Whether this invocation will touch ChirpStack.
///
/// Callers use it to avoid connecting on a Hub that runs no ChirpStack — the mode gate's whole
/// point is that a P2P-only Hub never waits on a service it does not have.
pub fn needs_chirpstack(cli: &ControlRadio, config: &RadioConfig) -> bool {
    if !config.mode.allows(Transport::Lorawan) {
        return false;
    }
    // `node-move` names a target, not a `--radio` arm, and LoRaWAN is one end of every move: the
    // Node's ChirpStack device goes when it leaves LoRaWAN and comes when it arrives. So it needs
    // ChirpStack wherever the mode has LoRaWAN, `dual` included, where `resolve` below refuses a
    // command with no `--radio` and a move off LoRaWAN was refused as unreachable.
    if matches!(cli.command, Command::NodeMove(_)) {
        return true;
    }
    // §5: `node-info` is this component's own bookkeeping and makes no RPC, so it must answer
    // on a Hub whose ChirpStack is down — which is exactly when someone checks what a device
    // was provisioned with. Connecting first would turn that read into `ServiceDown`.
    //
    // `node-get` is the same kind of read one layer out: it returns what the *device* reported
    // over radio, out of the local cache, and §5 is explicit that it "only ever reads what
    // this engine has already received". ChirpStack is not in that path at all, and its being
    // down is precisely when an operator wants the last thing a Node said.
    if matches!(cli.command, Command::NodeInfo(_) | Command::NodeGet(_)) {
        return false;
    }
    // `create --role client` is the cluster join, which the agent runs; it makes no gRPC call.
    if let Command::Create(a) = &cli.command {
        if a.role_client() {
            return false;
        }
    }
    // The boot `create` runs both arms under `dual` (see `run_with_store`), LoRaWAN first.
    if let Command::Create(a) = &cli.command {
        if config.mode == Mode::Dual && a.radio.transport().is_none() {
            return true;
        }
    }
    match resolve(
        config.mode,
        cli.command.requested_transport(),
        cli.command.radio_arity(),
    ) {
        Ok(Transport::Lorawan) => true,
        // `status` reports both arms, so it reads ChirpStack when the mode has it.
        _ => matches!(cli.command, Command::Status(_)),
    }
}

/// A status document `proximos-control` mirrors under `/run/proximos/radio/` (beside the
/// heartbeat), or `None` when it has not written one — a degraded read, like the rest of
/// `status`. Public for `control-core`, which derives `system/info.lorawan` from the LoRaWAN
/// one on a Hub where this crate owns the radio.
pub fn read_run_document(name: &str) -> Option<serde_json::Value> {
    let dir = crate::tasks::heartbeat::alive_path();
    let raw = std::fs::read(dir.parent()?.join(name)).ok()?;
    serde_json::from_slice(&raw).ok()
}
