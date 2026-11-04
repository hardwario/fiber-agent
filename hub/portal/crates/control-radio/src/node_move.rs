//! `node-move` — a Node changes carrier (`plan/control/radio/node_registry.md` §4, R4).
//!
//! The registry record keeps the Node's identity, keys and history, so a move deletes nothing;
//! it flips `radio` and changes what each carrier holds:
//!
//! 1. **The old carrier lets go.** LoRaWAN: its ChirpStack device is deleted. P2P: its session
//!    is parked, its queued downlinks are dropped, and its JoinRequest is no longer admitted
//!    (§4, amended 2026-09-28, Q-N4). If the old carrier is switched on and cannot let go
//!    (ChirpStack unreachable), the move is refused and nothing changes (§10 Q2). If
//!    `radio.mode` has it switched off, the mirror catches up when it is switched on.
//! 2. **The registry flips.**
//! 3. **The new carrier takes it.** LoRaWAN: a ChirpStack device with the Node's keys, when the
//!    arm is on; otherwise the mirror creates it at the next `create`. P2P: nothing to do — the
//!    central accepts the Node's next frame under a parked session, or admits its next
//!    JoinRequest.
//! 4. **The retained `info` is republished** under the new carrier, so the Portal shows the
//!    move at once. The LoRaWAN session the cache kept from the last join (`dev_addr`,
//!    `joined_at`) is cleared on the way, so neither carrier's `info` shows it again.
//!
//! The Node itself is not told: a downlink `radio-mode` write would sever the link that carries
//! it (#271, §10 Q1). The operator switches it by NFC or shell.

use crate::cli::NodeMoveArgs;
use crate::config::RadioConfig;
use crate::dispatch::Outcome;
use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::client::ChirpStack;
use crate::lorawan::enroll::KeyDisclosure;
use crate::lorawan::token::Tokens;
use crate::mode::Mode;
use crate::northbound::{self, Area, RetainedPublisher, Topics};
use crate::registry::{self, NodeRecord};
use crate::status::CommandResult;
use crate::store::{Slot, Store};

fn err(kind: Kind, transport: impl Into<Option<Transport>>, msg: String) -> Error {
    Error::new(kind, transport, Component::NodeRegistry, msg)
}

/// The Node a `--serial`/`--dev-eui` names, from the registry.
fn resolve(reg: &registry::Registry, args: &NodeMoveArgs) -> Result<NodeRecord> {
    let found = match (args.ident.serial.as_deref(), args.ident.dev_eui.as_deref()) {
        (Some(s), None) => reg.by_serial(s),
        (None, Some(e)) => reg.get(&registry::canonical_dev_eui(e)?),
        _ => {
            return Err(Error::new(
                Kind::InvalidArgument,
                None,
                Component::ControlSocket,
                "node-move needs exactly one of --serial or --dev-eui".to_string(),
            ))
        }
    };
    found.cloned().ok_or_else(|| {
        err(
            Kind::NotFound,
            None,
            "no Node registered by that identifier".into(),
        )
    })
}

/// The retained `info` document of `rec` on its carrier, in the shape that carrier's
/// `node-add` publishes.
pub fn registered_info(rec: &NodeRecord) -> Result<serde_json::Value> {
    Ok(match rec.radio {
        Transport::P2p => {
            let node = rec.as_registered().ok_or_else(|| {
                err(
                    Kind::InvalidArgument,
                    Transport::P2p,
                    format!("{} has no app_key, so it cannot join over P2P", rec.dev_eui),
                )
            })?;
            let mut info = node.info_core();
            info.insert("pairing_status".into(), "registered".into());
            serde_json::Value::Object(info)
        }
        Transport::Lorawan => rec.as_enrollment()?.to_json(KeyDisclosure::Omitted),
    })
}

pub async fn run(
    config: &RadioConfig,
    store: &Store,
    args: &NodeMoveArgs,
    client: Option<&dyn ChirpStack>,
    publisher: Option<&dyn RetainedPublisher>,
) -> Result<Outcome> {
    if config.mode == Mode::Off {
        return Err(Error::new(
            Kind::OffMode,
            None,
            Component::RadioMode,
            "radio.mode is off: no carrier to move a Node between".to_string(),
        ));
    }
    let to: Transport = args.to.into();
    let reg = registry::current(store)?;
    let rec = resolve(&reg, args)?;
    let from = rec.radio;
    let mut notes: Vec<String> = Vec::new();

    if from != to {
        if to == Transport::P2p && rec.app_key.is_none() {
            return Err(err(
                Kind::InvalidArgument,
                to,
                format!(
                    "{} has no app_key in the registry, so it cannot join over P2P; remove it \
                     and node-add it with its keys",
                    rec.dev_eui
                ),
            ));
        }
        if to == Transport::P2p {
            crate::p2p::tower::check_address_free(
                &rec.dev_eui,
                reg.on(Transport::P2p).map(|n| n.dev_eui.as_str()),
            )?;
        }
        // The publisher is demanded before anything changes, as node-remove does: a move
        // whose `info` cannot be republished would leave the Portal showing the old carrier.
        let broker = publisher.ok_or_else(|| {
            Error::new(
                Kind::ServiceDown,
                to,
                Component::MqttBroker,
                "no broker connection: node-move republishes the retained info".to_string(),
            )
        })?;
        let eui = crate::lorawan::eui::Eui::parse(&rec.dev_eui)?;

        // What the new carrier needs is worked out before anything changes, so a Node whose
        // profile LoRaWAN does not have is refused rather than left half-moved.
        let lorawan_target = match (to, config.mode.allows(Transport::Lorawan), client) {
            (Transport::Lorawan, true, Some(c)) => {
                let tokens = Tokens::new(store.clone());
                let state = crate::lorawan::converge::Converger {
                    client: c,
                    config,
                    tokens: &tokens,
                    store,
                }
                .load_state()?;
                let app = state.application_id()?.to_string();
                let mut moved = rec.clone();
                moved.radio = to;
                let target = crate::lorawan::mirror::device_for(&moved, &state, &app)
                    .map_err(|why| err(Kind::InvalidArgument, Transport::Lorawan, why))?;
                Some((c, target))
            }
            (Transport::Lorawan, _, _) => {
                notes.push(
                    "LoRaWAN is off on this Hub: its ChirpStack device is created when LoRaWAN \
                     is switched on"
                        .into(),
                );
                None
            }
            _ => None,
        };

        // 1. The old carrier lets go.
        if from == Transport::Lorawan {
            if config.mode.allows(Transport::Lorawan) {
                let c = client.ok_or_else(|| {
                    Error::new(
                        Kind::ServiceDown,
                        Transport::Lorawan,
                        Component::ChirpstackService,
                        "ChirpStack is not reachable, so the Node's LoRaWAN device cannot be \
                         removed; the move is refused and nothing changed"
                            .to_string(),
                    )
                })?;
                match c.delete_device(&eui).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == Kind::NotFound => {}
                    Err(e) => return Err(e),
                }
            } else {
                notes.push(
                    "LoRaWAN is off on this Hub: its ChirpStack device goes when LoRaWAN is \
                     switched on"
                        .into(),
                );
            }
        }

        // 2. The registry flips. From here the P2P view no longer holds the Node, so its
        // session is parked: the central decrypts and answers nothing under it.
        let rec = registry::set_radio(store, &rec.dev_eui, to)?;

        // The session is kept, not dropped (§4, amended 2026-09-28, Q-N4). The Node keeps it
        // in NVS, and a Node moved back went on using it while the Hub, which had dropped it,
        // ignored its frames until the Node rejoined (bench A4-2). Its queue and delivery
        // state go: they were sealed for the P2P link.
        if from == Transport::P2p {
            let _db_lock = store.lock(Slot::P2pSessions)?;
            let mut db = crate::p2p::db::SessionsDb::load(store)?;
            if db.park_session(&rec.dev_eui) {
                notes.push(
                    "its P2P session is parked and its queued P2P downlinks were dropped; a \
                     move back to p2p takes the session up again"
                        .into(),
                );
            }
            db.save(store)?;
        }
        // The LoRaWAN session in the cache is not the one the Node joins with next: off
        // LoRaWAN it belongs to the carrier the Node left, and back onto LoRaWAN to a
        // ChirpStack device an earlier move deleted (node_registry.md §5, amended 2026-09-28).
        // Logged rather than fatal: the registry has flipped, and a stale `dev_addr` on the
        // Portal is not worth a half-done move.
        if let Err(e) = crate::cache::Cache::new(store, to).clear_lorawan_session(&rec.dev_eui) {
            eprintln!(
                "[registry] {}: cannot clear its LoRaWAN session from the cache: {e}",
                rec.dev_eui
            );
        }

        // 3. The new carrier takes it.
        if let Some((c, (spec, keys))) = lorawan_target {
            if c.get_device(&spec.dev_eui).await?.is_none() {
                c.create_device(&spec).await?;
            }
            if c.get_device_keys(&spec.dev_eui).await?.is_some() {
                c.update_device_keys(&spec.dev_eui, &keys).await?;
            } else {
                c.create_device_keys(&spec.dev_eui, &keys).await?;
            }
        }

        // 4. The Portal learns the move from the retained info.
        let topic = Topics::new(northbound::hostname()).node(&rec.dev_eui, Area::Info);
        let payload = serde_json::to_vec(&registered_info(&rec)?)
            .map_err(|e| err(Kind::StorageError, to, e.to_string()))?;
        broker.publish_retained(&[(topic, payload)]).await?;
        eprintln!(
            "[registry] {} moved from {} to {}",
            rec.dev_eui,
            from.as_str(),
            to.as_str()
        );
    }

    let mut result = CommandResult::new("node-move", to)
        .with("dev_eui", rec.dev_eui.as_str())
        .with("serial_number", rec.serial.as_str())
        .with("from", from.as_str())
        .with("to", to.as_str())
        .with("state", if from == to { "unchanged" } else { "moved" })
        .with(
            "next_step",
            format!(
                "switch the Node itself to {} (NFC or shell radio-mode)",
                to.as_str()
            ),
        );
    if !notes.is_empty() {
        result = result.with("notes", notes);
    }
    Ok(Outcome::Json(
        serde_json::to_value(result).map_err(|e| err(Kind::StorageError, to, e.to_string()))?,
    ))
}

/// `node-remove` of a Node whose carrier this Hub has switched off (node_registry.md §4,
/// amended 2026-09-27: „ano, povolit jeho smazání“). `None` when the registry does not hold
/// the Node, or its carrier is on: the carrier's own `node-remove` arm handles those.
///
/// The Node is removed the way its carrier would remove it, as far as the Hub can without that
/// carrier running:
/// - the retained topics are tombstoned first, so a broker failure leaves the command
///   re-runnable;
/// - P2P: its session goes from `sessions.db`, a local file;
/// - LoRaWAN: its DevEUI goes on the registry's `removed` list, and the mirror deletes the
///   ChirpStack device once LoRaWAN is on. A P2P session it parked when it moved off P2P goes
///   too (§4, amended 2026-09-28);
/// - the registry record and the cache go.
pub async fn remove_on_switched_off_carrier(
    config: &RadioConfig,
    store: &Store,
    ident: &crate::cli::NodeIdent,
    publisher: Option<&dyn RetainedPublisher>,
) -> Result<Option<Outcome>> {
    let reg = registry::current(store)?;
    let found = match (ident.serial.as_deref(), ident.dev_eui.as_deref()) {
        (Some(s), None) => reg.by_serial(s),
        (None, Some(e)) => match registry::canonical_dev_eui(e) {
            Ok(id) => reg.get(&id),
            Err(_) => None,
        },
        _ => None,
    };
    let Some(rec) = found.cloned() else {
        return Ok(None);
    };
    if config.mode.allows(rec.radio) {
        return Ok(None);
    }
    let broker = publisher.ok_or_else(|| {
        Error::new(
            Kind::ServiceDown,
            rec.radio,
            Component::MqttBroker,
            "no broker connection: node-remove has to clear the retained node inventory"
                .to_string(),
        )
    })?;
    let stones: Vec<(String, Vec<u8>)> = Topics::new(northbound::hostname())
        .tombstones(&rec.dev_eui)
        .into_iter()
        .map(|t| (t, northbound::TOMBSTONE.to_vec()))
        .collect();
    broker.publish_retained(&stones).await?;

    if rec.radio == Transport::P2p {
        let _db_lock = store.lock(Slot::P2pSessions)?;
        let mut db = crate::p2p::db::SessionsDb::load(store)?;
        db.remove_node(&rec.dev_eui);
        db.sessions.remove(&rec.dev_eui);
        db.save(store)?;
    } else {
        crate::p2p::db::drop_parked_or_log(store, std::slice::from_ref(&rec.dev_eui));
    }
    registry::remove_off_carrier(store, &rec.dev_eui)?;
    if rec.radio == Transport::Lorawan {
        // Rewrite the rollback mirror without the Node.
        crate::lorawan::enroll::NodesFile::load(store)?.save(store)?;
    }
    crate::cache::Cache::new(store, rec.radio).remove(&rec.dev_eui)?;
    eprintln!(
        "[registry] {} removed while {} is off on this Hub",
        rec.dev_eui,
        rec.radio.as_str()
    );

    let note = match rec.radio {
        Transport::Lorawan => {
            "LoRaWAN is off on this Hub: its ChirpStack device is deleted when \
                               LoRaWAN is switched on"
        }
        Transport::P2p => "P2P is off on this Hub: its session was dropped",
    };
    let result = CommandResult::new("node-remove", rec.radio)
        .with("serial_number", rec.serial.as_str())
        .with("dev_eui", rec.dev_eui.as_str())
        .with("state", "deleted")
        .with("notes", vec![note.to_string()]);
    Ok(Some(Outcome::Json(serde_json::to_value(result).map_err(
        |e| err(Kind::StorageError, rec.radio, e.to_string()),
    )?)))
}
