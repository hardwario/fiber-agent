//! In-process `control.radio` seam (plan/portal/p2p_integration.md, PI2).
//!
//! The Portal reaches the v2 radio verbs by calling `control_radio::dispatch` **in-process** —
//! not over MQTT, and not through the v1 `fiber_app` agent. Authorization is the control-socket
//! trust model without the socket: the caller is already the local `proximos` user and the
//! route writes an audit row (§2.1). No signed envelope is built — a signature verified inside
//! the same process proves nothing (Decision #10 puts the privilege boundary at the portal's
//! role gate + audit row).
//!
//! This is the seam the LoRaWAN config path migrates onto, replacing the signed-MQTT round-trip
//! to `fiber_app` (`set_sticker_config`/`get_sticker_config`). It is carried on `AppState` and
//! is cheap to clone (an `Arc` inside).

pub(crate) mod alarm;
mod errors;
mod inproc;

pub use errors::RadioError;

use std::sync::Arc;

use control_radio::cli::{NodeSendCmdArgs, NodeSendKind, RadioArg};
use control_radio::config::RadioConfig;
use control_radio::dispatch::{self, Outcome};
use control_radio::error::Transport;
use control_radio::lorawan::tonic_client::TonicChirpStack;
use control_radio::lorawan::Tokens;
use control_radio::mqtt::RumqttcPublisher;
use control_radio::store::Store;
use control_radio::Mode;
use portal_domain::Transport as Carrier;

/// Where this Hub's registry holds a Node, and the carrier the Portal reaches it over:
/// [`RadioControl::carrier_for`], the one answer to "which carrier is this Node on?"
/// (decision #23). Registration, removal, "Změnit rádio", the audit rows, the send walk's P-7
/// rule and the draft's downlink count all ask it, rather than each reading the retained
/// `info`'s `transport` or `radio.mode` on its own.
///
/// One registry holds every Node on exactly one carrier (node_registry.md), so there is no "on
/// both carriers" to resolve any more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeCarrier {
    /// The carrier commands go over: the registry's `radio` for a Node it holds. For one it does
    /// not hold yet, the one a registration goes out on by default: the Hub's `radio.mode`, and
    /// P2P on a `dual` Hub ([`registration_default`]).
    pub carrier: Carrier,
    /// This Hub's registry holds the Node.
    pub registered: bool,
    /// `radio.mode` has `carrier` switched on. A Node the registry keeps on a carrier the Hub has
    /// switched off (a LoRaWAN Node on a P2P Hub) is not reached until it is moved or the carrier
    /// is switched on (node_registry.md §4). `true` on a seam with no mode, which cannot tell.
    pub enabled: bool,
}

impl NodeCarrier {
    /// The carrier the registry already holds the Node on, which a registration must not
    /// duplicate. `None` for a Node the registry does not hold.
    pub fn registered_on(&self) -> Option<Carrier> {
        self.registered.then_some(self.carrier)
    }

    /// The registry holds the Node on a carrier this Hub has switched off: the page shows
    /// "radio disabled on this Hub" and offers "Změnit rádio", not an error.
    pub fn disabled(&self) -> bool {
        self.registered && !self.enabled
    }
}

/// "Přesunout všechny Nody na <rádio>" (node_registry.md §11; devices.hub.md, dodatek
/// 2026-09-27, R6): [`RadioControl::bulk_move`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkMove {
    /// The carrier the Hub has switched off, which the Nodes are on.
    pub from: Carrier,
    /// The one carrier the Hub runs, which they move to.
    pub to: Carrier,
    /// The registry's Nodes on `from`, in DevEUI order.
    pub dev_euis: Vec<String>,
}

/// What "Změnit rádio Hubu" asks about the Nodes (node_registry.md §12):
/// [`RadioControl::node_switch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSwitch {
    /// The registry's Nodes on a carrier the Hub runs now and the new mode switches off, each
    /// with that carrier, in DevEUI order.
    pub nodes: Vec<(String, Carrier)>,
    /// Where „převést na nové rádio“ moves them: the one carrier the new mode runs. `None` when
    /// it runs none (`off`), and then the move is not offered.
    pub move_to: Option<Carrier>,
}

/// control-radio's transport for a carrier.
fn transport_of(carrier: Carrier) -> Transport {
    match carrier {
        Carrier::P2p => Transport::P2p,
        Carrier::Lorawan => Transport::Lorawan,
    }
}

/// control-radio's `--radio` for a carrier.
fn radio_arg(carrier: Carrier) -> RadioArg {
    match carrier {
        Carrier::P2p => RadioArg::P2p,
        Carrier::Lorawan => RadioArg::Lorawan,
    }
}

/// What a Discard's withdrawal did ([`RadioControl::withdraw_queued`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Withdrawal {
    /// Frames taken out of the queue.
    pub dropped: usize,
    /// One of the frames had already reached the Node, or may have: it was delivered (P2P `Sent`)
    /// or is no longer queued (LoRaWAN: transmitted).
    pub maybe_delivered: bool,
}

/// A P2P address another Node already holds ([`RadioControl::p2p_address_collision`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressCollision {
    /// The address, as the Node's `dev_addr` reads: 8 uppercase hex digits.
    pub addr: String,
    /// The DevEUIs on P2P holding it, lowercase.
    pub holders: Vec<String>,
}

/// The in-process radio seam.
#[derive(Clone)]
pub struct RadioControl {
    inner: Arc<Inner>,
}

struct Inner {
    /// The rendered radio config, or `None` on a Hub with no LoRaWAN arm — a P2P-only or
    /// radio-off Hub, or a dev process without `/run/proximos/control/config.yaml`. `None`
    /// makes every command a clean [`RadioError::NotConfigured`], never a panic or a hang.
    config: Option<RadioConfig>,
    /// `radio.mode`, which [`RadioControl::carrier_for`] resolves from: the config's, or in a
    /// test a mode of its own beside no config.
    mode: Option<Mode>,
    /// Where the per-device sequence cache and the ChirpStack API tokens live
    /// (`PROXIMOS_RADIO_DATA`, else the shipped default) — the same root `proximosctl` uses.
    store_root: String,
}

impl RadioControl {
    /// Build from the process environment, exactly as `proximosctl run_radio` does: the
    /// rendered `/run/proximos/control/config.yaml` and `PROXIMOS_RADIO_DATA`.
    ///
    /// A missing or unparseable config is **not** fatal — the Portal still serves every other
    /// page; radio commands answer [`RadioError::NotConfigured`]. This mirrors
    /// `chirpstack.service`'s own `ConditionPathExists` gate: absence means "not asked for",
    /// not "broken".
    pub fn from_env() -> Self {
        let config = match dispatch::load_config() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::info!("radio seam inert (no rendered radio config): {e}");
                None
            }
        };
        Self::new(config, None, dispatch::store_root())
    }

    fn new(config: Option<RadioConfig>, mode: Option<Mode>, store_root: String) -> Self {
        let mode = config.as_ref().map(|c| c.mode).or(mode);
        Self {
            inner: Arc::new(Inner {
                config,
                mode,
                store_root,
            }),
        }
    }

    /// `false` when the seam is inert, so every command would answer
    /// [`RadioError::NotConfigured`]. The Hub pages read it to say so on the
    /// portal's own Hub instead of showing only the MQTT-reported health.
    pub fn is_configured(&self) -> bool {
        self.inner.config.is_some()
    }

    /// A seam for a test: `mode` and the registries under `store_root`, and no rendered config,
    /// so every command still answers [`RadioError::NotConfigured`] while
    /// [`Self::carrier_for`] resolves as on a real Hub.
    #[cfg(test)]
    pub(crate) fn for_tests(mode: Mode, store_root: &std::path::Path) -> Self {
        Self::new(None, Some(mode), store_root.to_string_lossy().into_owned())
    }

    /// An inert seam that reports `mode` as rendered, for route tests.
    #[cfg(test)]
    pub fn with_rendered_mode(mode: Option<Mode>) -> Self {
        Self::new(None, mode, dispatch::store_root())
    }

    /// This Hub's `radio.mode`, the one this boot was rendered with; `None` for a seam with no
    /// rendered config. `/setup` compares the operator's choice against it to tell whether the
    /// Hub is about to reboot into another mode, as `first-run-apply` does against the same file.
    pub fn mode(&self) -> Option<Mode> {
        self.inner.mode
    }

    /// The Nodes this Hub's registry keeps on P2P at `dev_eui`'s P2P address, the low 32 bits
    /// of the DevEUI (p2p_tower_gateway.md §10.1, #470 H4): the ones a registration or a move
    /// of `dev_eui` to P2P would collide with. `None` when none does. The Hub refuses the same
    /// with `AddressCollision`; the Portal asks first so it can name both Nodes in Czech before
    /// anything is signed, as it does for a DevEUI already registered.
    pub fn p2p_address_collision(&self, dev_eui: &str) -> Option<AddressCollision> {
        let dev_eui = dev_eui.to_ascii_lowercase();
        let addr = control_radio::p2p::tower::addr_of(&dev_eui)?;
        let registry = control_radio::registry::Registry::load(&Store::new(&self.inner.store_root))
            .ok()
            .flatten()?;
        let holders: Vec<String> = registry
            .on(Transport::P2p)
            .map(|n| n.dev_eui.to_ascii_lowercase())
            .filter(|other| {
                *other != dev_eui && control_radio::p2p::tower::addr_of(other) == Some(addr)
            })
            .collect();
        (!holders.is_empty()).then(|| AddressCollision {
            // The spelling of the Node's `dev_addr` (§16): 8 uppercase hex digits.
            addr: format!("{addr:08X}"),
            holders,
        })
    }

    /// Each P2P session's home gateway (p2p_cluster_gateway.md §5), keyed by lowercase DevEUI:
    /// `None` for this Hub's own Northbridge (`local`), else the Client's hostname. A Node with
    /// no session (not joined yet) is absent. Read from this Hub's `sessions.db`, the central's
    /// own file, never migrated. An unreadable or absent file answers an empty map: the Nodes
    /// list shows "—" rather than failing (devices.node.md §5, dodatek 2026-10-13).
    pub fn p2p_homes(&self) -> std::collections::HashMap<String, Option<String>> {
        control_radio::p2p::db::SessionsDb::load_file(&Store::new(&self.inner.store_root))
            .map(|db| {
                db.sessions
                    .into_values()
                    .map(|s| (s.dev_eui.to_ascii_lowercase(), s.home))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The P2P modulation this Hub's rendered `radio.p2p_radio` runs, for the Hub settings'
    /// read-only row (#470 H4; p2p_tower_gateway.md §15, amended 2026-10-07): `lora` until
    /// #470 P5. `None` when `radio.mode` runs no P2P. A P2P mode with no `p2p_radio` section
    /// (or no rendered config at all, in a test) runs the default, `lora`.
    pub fn p2p_modulation(&self) -> Option<&'static str> {
        use control_radio::config::P2pModulation;
        if !self.inner.mode?.allows(Transport::P2p) {
            return None;
        }
        let modulation = self
            .inner
            .config
            .as_ref()
            .and_then(|c| c.p2p_radio.as_ref())
            .map(|p| p.modulation)
            .unwrap_or_default();
        Some(match modulation {
            P2pModulation::Lora => "lora",
            P2pModulation::Fsk => "fsk",
        })
    }

    /// The carrier `dev_eui` is on, and whether this Hub's registry holds it: the one place the
    /// Portal decides a Node's carrier (decision #23, node_registry.md §6).
    ///
    /// The registry's `radio` decides on every Hub, a single-carrier one included: the registry
    /// may keep a Node on a carrier `radio.mode` has switched off, and that Node is shown as
    /// such rather than as one on the Hub's carrier ([`NodeCarrier::disabled`]). The registry is
    /// read, never migrated ([`registry_radio`]).
    ///
    /// The seam reads this Hub's own registry, so the answer is about the Hub the Portal runs on,
    /// the one every seam command goes to.
    ///
    /// Cheap, because it runs per command and per page: one small file.
    pub fn carrier_for(&self, dev_eui: &str) -> NodeCarrier {
        resolve_carrier(
            self.inner.mode,
            registry_radio(&Store::new(&self.inner.store_root), dev_eui),
        )
    }

    /// The Node's own carrier when this Hub's `radio.mode` has switched it off
    /// (node_registry.md §11): a `lorawan` record on a `p2p` Hub, any record on an `off` Hub.
    /// `None` while its carrier is on, for a Node the registry does not hold, and on a seam with
    /// no mode.
    ///
    /// [`Self::carrier_for`]'s [`NodeCarrier::disabled`], for the Node page's banner. A
    /// registration of such a Node is refused as registered, as of any Node the registry holds
    /// (node_registry.md §11, amended R7): it moves with "Změnit rádio" (`node-move`).
    pub fn carrier_switched_off(&self, dev_eui: &str) -> Option<Carrier> {
        switched_off(
            self.inner.mode?,
            &Store::new(&self.inner.store_root),
            dev_eui,
        )
    }

    /// The bulk move the Hub card offers after a mode change: every Node the registry keeps on
    /// the carrier this Hub has switched off, to the one carrier it runs. `None` on a `dual` Hub
    /// (both run, so none is off) and an `off` one (nothing to move to), on a seam with no mode,
    /// and when no Node is on the switched-off carrier. Read from the registry, never migrating
    /// it, like [`Self::carrier_for`].
    pub fn bulk_move(&self) -> Option<BulkMove> {
        let to = match self.inner.mode? {
            Mode::P2p => Carrier::P2p,
            Mode::Lorawan => Carrier::Lorawan,
            Mode::Dual | Mode::Off => return None,
        };
        let from = to.other();
        let registry = control_radio::registry::Registry::load(&Store::new(&self.inner.store_root))
            .ok()
            .flatten()?;
        let dev_euis: Vec<String> = registry
            .on(transport_of(from))
            .map(|n| n.dev_eui.clone())
            .collect();
        (!dev_euis.is_empty()).then_some(BulkMove { from, to, dev_euis })
    }

    /// The Nodes a change of this Hub's `radio.mode` to `to` leaves on a carrier it switches
    /// off, and where a move would take them (node_registry.md §12). `None` when there is nothing
    /// to ask: no carrier goes off (`p2p` → `dual`), no Node is on one that does, or a seam with
    /// no mode.
    ///
    /// A carrier "goes off" when the Hub runs it now and `to` does not. A Node already on a
    /// carrier the Hub had switched off before is not asked about again: it was left there by an
    /// earlier choice, and "Přesunout všechny Nody" is its action. Read from the registry, never
    /// migrating it, like [`Self::bulk_move`].
    pub fn node_switch(&self, to: Mode) -> Option<NodeSwitch> {
        let from = self.inner.mode?;
        let off: Vec<Carrier> = [Carrier::Lorawan, Carrier::P2p]
            .into_iter()
            .filter(|c| from.allows(transport_of(*c)) && !to.allows(transport_of(*c)))
            .collect();
        if off.is_empty() {
            return None;
        }
        let registry = control_radio::registry::Registry::load(&Store::new(&self.inner.store_root))
            .ok()
            .flatten()?;
        let mut nodes: Vec<(String, Carrier)> = off
            .iter()
            .flat_map(|c| {
                registry
                    .on(transport_of(*c))
                    .map(|n| (n.dev_eui.clone(), *c))
                    .collect::<Vec<_>>()
            })
            .collect();
        nodes.sort_by(|a, b| a.0.cmp(&b.0));
        let move_to = match to {
            Mode::P2p => Some(Carrier::P2p),
            Mode::Lorawan => Some(Carrier::Lorawan),
            // `dual` switches nothing off, so it never gets here; `off` has nowhere to go.
            Mode::Dual | Mode::Off => None,
        };
        (!nodes.is_empty()).then_some(NodeSwitch { nodes, move_to })
    }

    /// Ask a node for the small settings-info it sends after boot — intervals, history, the
    /// `cap_*` sensors and the 1-Wire slot types.
    ///
    /// Enqueues one fPort-85 `GetSettings` downlink (`Command` field 31, sticker-firmware
    /// #429). The one-frame answer arrives when the command does (the next uplink on LoRaWAN,
    /// the second on P2P), merges into what the Hub
    /// holds and lands on the retained `config` topic. **Never a full `get-config`**: that
    /// answer runs to 6–14 pages, too big for the portal to ask for (decision 2026-09-23,
    /// `node_reported_state.md` §2.3). The service CLI keeps `get-config` for full reads.
    pub async fn get_settings(&self, dev_eui: &str) -> Result<serde_json::Value, RadioError> {
        self.node_send(dev_eui, NodeSendKind::GetSettings).await
    }

    /// Ask a node to report its device info (`node-send get-info`, fPort-85 `GetInfo`). The
    /// answer lands on the retained `device_info` topic and repaints the Device card.
    pub async fn get_info(&self, dev_eui: &str) -> Result<serde_json::Value, RadioError> {
        self.node_send(dev_eui, NodeSendKind::GetInfo).await
    }

    /// Ask a P2P node for its device status (`node-send dev-status`, a `0x91` DevStatusReq,
    /// p2p_control_commands.md §5). The answer lands on the retained `device_status` topic.
    /// control.radio refuses it for a LoRaWAN node or one without the Capabilities bit.
    pub async fn dev_status(&self, dev_eui: &str) -> Result<serde_json::Value, RadioError> {
        self.node_send(dev_eui, NodeSendKind::DevStatus).await
    }

    /// Reboot a node (`node-send cmd reboot`). It restarts at its next RX window and rejoins;
    /// its boot `Info` and settings-info then refresh the Device and config cards.
    pub async fn reboot(&self, dev_eui: &str) -> Result<serde_json::Value, RadioError> {
        self.node_send(
            dev_eui,
            NodeSendKind::Cmd(NodeSendCmdArgs {
                name: "reboot".to_string(),
                from: None,
                to: None,
                counter: Vec::new(),
            }),
        )
        .await
    }

    /// Ask a node for an extra measurement uplink now (`node-send cmd force-send`). The
    /// firmware answers nothing on fPort 85 (HIL #432): the fPort-2 reading that follows is
    /// the confirmation (nodes_phase1.cs.md WP6).
    pub async fn force_send(&self, dev_eui: &str) -> Result<serde_json::Value, RadioError> {
        self.node_send(
            dev_eui,
            NodeSendKind::Cmd(NodeSendCmdArgs {
                name: "force-send".to_string(),
                from: None,
                to: None,
                counter: Vec::new(),
            }),
        )
        .await
    }

    /// Ask a node to replay the records it holds for `[from, to]`, unix seconds
    /// (`node-send cmd history-replay`, fPort-85 `ReqHistory`).
    ///
    /// The Node answers with a stream of `HistoryFrame`s, which control-radio decodes onto the
    /// node's `history` topic for the persist sink to store and credit (p2p_history_replay.md
    /// §14); a window it holds nothing for is answered `HISTORY_UNAVAILABLE`. The answer names
    /// the `seq` the downlink went out under, which is how a refusal is matched to the request.
    pub async fn history_replay(
        &self,
        dev_eui: &str,
        from: i64,
        to: i64,
    ) -> Result<serde_json::Value, RadioError> {
        self.node_send(
            dev_eui,
            NodeSendKind::Cmd(NodeSendCmdArgs {
                name: "history-replay".to_string(),
                from: Some(from),
                to: Some(to),
                counter: Vec::new(),
            }),
        )
        .await
    }

    /// Take the frames queued under `seqs` back out of the node's ChirpStack queue, keeping
    /// everything else queued in order (P-6, nodes_phase1.cs.md dodatek 2026-09-25): a
    /// discarded draft must not reach the Node at its next uplink. Returns how many frames
    /// were withdrawn.
    pub async fn drop_queued(&self, dev_eui: &str, seqs: &[u64]) -> Result<usize, RadioError> {
        if self.inner.config.is_none() {
            return Err(RadioError::NotConfigured(
                "no rendered radio config".to_string(),
            ));
        }
        if self.carrier_for(dev_eui).carrier == Carrier::P2p {
            // The P2P queue lives in `sessions.db`, which the central re-reads per event; the
            // slot lock keeps its save from racing this one, as with `node-send` itself.
            let store = Store::new(&self.inner.store_root);
            let _lock = store.lock(control_radio::store::Slot::P2pSessions)?;
            let mut db = control_radio::p2p::db::SessionsDb::load(&store)?;
            let seqs: Vec<u32> = seqs.iter().filter_map(|s| u32::try_from(*s).ok()).collect();
            let key = db
                .sessions
                .keys()
                .find(|k| k.eq_ignore_ascii_case(dev_eui))
                .cloned()
                .unwrap_or_else(|| dev_eui.to_ascii_lowercase());
            let dropped = db.drop_queued_seqs(&key, &seqs);
            if dropped > 0 {
                db.save(&store)?;
            }
            return Ok(dropped);
        }
        let eui = control_radio::lorawan::eui::Eui::parse(dev_eui).map_err(RadioError::Dispatch)?;
        let tokens = Tokens::new(Store::new(&self.inner.store_root));
        let client = TonicChirpStack::connect(tokens).await?;
        let seqs: Vec<u32> = seqs.iter().filter_map(|s| u32::try_from(*s).ok()).collect();
        Ok(control_radio::lorawan::queue::drop_queued_seqs(&client, &eui, &seqs).await?)
    }

    /// A Discard's withdrawal (PF-3b, nodes_phase1.cs.md dodatek 2026-09-27): the frames queued
    /// under `seqs` go **including one already delivered** (a P2P head in `Sent`), and the answer
    /// says whether the Node may have received one of them. [`Self::drop_queued`], which keeps a
    /// delivered head, stays for the history retry and the portal's resend.
    pub async fn withdraw_queued(
        &self,
        dev_eui: &str,
        seqs: &[u64],
    ) -> Result<Withdrawal, RadioError> {
        if self.inner.config.is_none() {
            return Err(RadioError::NotConfigured(
                "no rendered radio config".to_string(),
            ));
        }
        let wanted: Vec<u32> = seqs.iter().filter_map(|s| u32::try_from(*s).ok()).collect();
        if self.carrier_for(dev_eui).carrier == Carrier::P2p {
            let store = Store::new(&self.inner.store_root);
            let _lock = store.lock(control_radio::store::Slot::P2pSessions)?;
            let mut db = control_radio::p2p::db::SessionsDb::load(&store)?;
            let key = db
                .sessions
                .keys()
                .find(|k| k.eq_ignore_ascii_case(dev_eui))
                .cloned()
                .unwrap_or_else(|| dev_eui.to_ascii_lowercase());
            let (dropped, sent_head) = db.withdraw_seqs(&key, &wanted);
            if dropped > 0 {
                db.save(&store)?;
            }
            // A frame not found was delivered and retired, or flushed: the Node may have it.
            return Ok(Withdrawal {
                dropped,
                maybe_delivered: sent_head || dropped < wanted.len(),
            });
        }
        let eui = control_radio::lorawan::eui::Eui::parse(dev_eui).map_err(RadioError::Dispatch)?;
        let tokens = Tokens::new(Store::new(&self.inner.store_root));
        let client = TonicChirpStack::connect(tokens).await?;
        let (dropped, maybe_delivered) =
            control_radio::lorawan::queue::withdraw_queued_seqs(&client, &eui, &wanted).await?;
        Ok(Withdrawal {
            dropped,
            maybe_delivered,
        })
    }

    /// Register an external LoRaWAN gateway in the local ChirpStack, or remove one:
    /// control.radio's `gateway-add` / `gateway-remove`, the CLI's own dispatch. Backend
    /// preparation only (Hynek 2026-09-26): no page calls it yet, and it is **not tested on
    /// hardware**. control.radio answers `BridgeDisabled` for `gateway-add` until the
    /// gateway-bridge opt-in exists (lorawan.md §7), and that refusal is returned as it is.
    pub async fn gateway(
        &self,
        gateway_eui: &str,
        name: Option<&str>,
        remove: bool,
    ) -> Result<serde_json::Value, RadioError> {
        use control_radio::cli::{
            Command, ControlRadio, GatewayAddArgs, GatewayRemoveArgs, RadioArg, RadioSelector,
        };
        let config = self
            .inner
            .config
            .as_ref()
            .ok_or_else(|| RadioError::NotConfigured("no rendered radio config".to_string()))?;
        let radio = RadioSelector {
            radio: Some(RadioArg::Lorawan),
        };
        let command = if remove {
            Command::GatewayRemove(GatewayRemoveArgs {
                radio,
                gateway_eui: Some(gateway_eui.to_string()),
            })
        } else {
            Command::GatewayAdd(GatewayAddArgs {
                radio,
                gateway_eui: Some(gateway_eui.to_string()),
                name: name.map(str::to_string),
            })
        };
        let cli = ControlRadio {
            json: true,
            command,
        };
        let store = Store::new(&self.inner.store_root);
        let client = TonicChirpStack::connect(Tokens::new(store.clone())).await?;
        let outcome = dispatch::run_with_store(
            &cli,
            config,
            Some(&client as &dyn control_radio::lorawan::client::ChirpStack),
            None,
            None,
            store,
        )
        .await
        .map_err(RadioError::Dispatch)?;
        Ok(match outcome {
            Outcome::Json(v) => v,
            Outcome::Text(t) => serde_json::json!({ "message": t }),
            Outcome::Status(_) => serde_json::json!({}),
        })
    }

    /// Write device-config values to a node.
    ///
    /// `assignments` are `group.field=value` — control.radio's own set-config vocabulary,
    /// packed into as few fPort-85 `SetParam` downlinks as the Node's data rate allows. The node
    /// applies them and echoes the new config on its retained `config` topic. This replaces the
    /// v1 `set_sticker_config` signed-relay round-trip to `fiber_app`.
    ///
    /// `replace_alarms` clears every alarm slot first (firmware `SetParam.alarms_replace`): the
    /// portal owns the alarm table and rewrites it whole (nodes_phase1.cs.md WP2).
    ///
    /// `supersede` marks the write as the Node's whole pending configuration (config_sync.md
    /// §13 Q20b): the Hub withdraws an earlier such write still waiting in the queue.
    pub async fn set_config(
        &self,
        dev_eui: &str,
        assignments: Vec<String>,
        replace_alarms: bool,
        supersede: bool,
    ) -> Result<serde_json::Value, RadioError> {
        self.node_send(
            dev_eui,
            NodeSendKind::SetConfig {
                assignments,
                replace_alarms,
                supersede,
            },
        )
        .await
    }

    /// Run one structured `node-send`, connecting to the local ChirpStack only for a LoRaWAN
    /// node.
    ///
    /// The client used to be built before every command. On a P2P Hub ChirpStack does not run,
    /// so every Portal command to a P2P Node failed `ServiceDown` before the transport was even
    /// resolved (P2P-N4/N12, bench 2026-09-26): refresh config, read device, drafts, backfill,
    /// reboot. A P2P send queues into `sessions.db` and needs no client. A long-lived pooled
    /// client is a later optimisation, not a correctness requirement.
    async fn node_send(
        &self,
        dev_eui: &str,
        what: NodeSendKind,
    ) -> Result<serde_json::Value, RadioError> {
        let config = self
            .inner
            .config
            .as_ref()
            .ok_or_else(|| RadioError::NotConfigured("no rendered radio config".to_string()))?;

        let at = self.carrier_for(dev_eui);
        // A Node the registry keeps on a carrier this Hub has switched off (node_registry.md
        // §4): the mode gate's own refusal, rather than a ChirpStack connection a P2P Hub does
        // not have or a P2P lookup that says the Node is unknown.
        if at.disabled() {
            control_radio::mode::resolve(
                config.mode,
                Some(transport_of(at.carrier)),
                control_radio::mode::RadioArity::Unified,
            )?;
        }
        let transport = radio_arg(at.carrier);
        // Named only where `radio.mode` cannot resolve it; elsewhere the CLI's own resolution.
        let radio = (config.mode == Mode::Dual).then_some(transport);
        // The Hub's broker, for the one thing a `node-send` publishes: the `superseded`
        // `command` document of a frame a newer identical ask withdrew (Q-N1). It connects only
        // then, as `proximosctl`'s does.
        let broker = RumqttcPublisher::new();
        let outcome = match transport {
            RadioArg::P2p => {
                inproc::run_node_send(
                    config,
                    &self.inner.store_root,
                    None,
                    Some(&broker),
                    dev_eui,
                    what,
                    radio,
                )
                .await?
            }
            RadioArg::Lorawan => {
                let tokens = Tokens::new(Store::new(&self.inner.store_root));
                let client = TonicChirpStack::connect(tokens).await?;
                inproc::run_node_send(
                    config,
                    &self.inner.store_root,
                    Some(&client),
                    Some(&broker),
                    dev_eui,
                    what,
                    radio,
                )
                .await?
            }
        };

        Ok(match outcome {
            Outcome::Json(v) => v,
            // A structured `node-send` always renders as `Outcome::Json`; the other two arms
            // belong to other commands. Fold them into a shape the caller can still render.
            Outcome::Text(t) => serde_json::json!({ "message": t }),
            Outcome::Status(_) => serde_json::json!({}),
        })
    }
}

/// [`RadioControl::carrier_for`] without the store: `held` is the registry's `radio` for the
/// Node, `None` for a Node it does not hold.
fn resolve_carrier(mode: Option<Mode>, held: Option<Transport>) -> NodeCarrier {
    let carrier = match held {
        Some(t) => carrier_of(t),
        None => registration_default(mode),
    };
    NodeCarrier {
        carrier,
        registered: held.is_some(),
        enabled: carrier_enabled(mode, carrier),
    }
}

/// [`RadioControl::carrier_switched_off`] against one store.
fn switched_off(mode: Mode, store: &Store, dev_eui: &str) -> Option<Carrier> {
    let at = resolve_carrier(Some(mode), registry_radio(store, dev_eui));
    at.disabled().then_some(at.carrier)
}

/// The registry's `radio` for `dev_eui`, `None` for a Node it does not hold. Read from the
/// registry file alone, never migrating it: a page view does not write the Hub's registry, and
/// `proximos-control` migrates it at its start. An unreadable registry reads as not holding the
/// Node.
fn registry_radio(store: &Store, dev_eui: &str) -> Option<Transport> {
    control_radio::registry::Registry::load(store)
        .ok()
        .flatten()?
        .get(dev_eui)
        .map(|n| n.radio)
}

/// The carrier a registration goes out on when the dialog names none: the Hub's `radio.mode`,
/// P2P on a `dual` Hub (Hynek 2026-09-27, the carrier decision #16 prefers), and LoRaWAN on a
/// radio-off Hub and a seam with no mode — the choice the registration dialog preselects
/// (devices.node.md, dodatek 2026-09-27).
///
/// The rule itself is `control_radio::mode::registration_default`, which the Hub's BLE FB0D
/// Node add uses too (GitLab #118); this only names the carrier in the Portal's type.
pub fn registration_default(mode: Option<Mode>) -> Carrier {
    carrier_of(control_radio::mode::registration_default(mode))
}

/// The Portal's carrier for control-radio's transport; [`transport_of`] the other way.
fn carrier_of(transport: Transport) -> Carrier {
    match transport {
        Transport::P2p => Carrier::P2p,
        Transport::Lorawan => Carrier::Lorawan,
    }
}

/// `radio.mode` has `carrier`'s arm switched on. `true` for a seam with no mode: it cannot tell,
/// and the Hub's own gate answers.
pub fn carrier_enabled(mode: Option<Mode>, carrier: Carrier) -> bool {
    mode.is_none_or(|m| m.allows(transport_of(carrier)))
}

/// A `{group.field: value}` config object as control.radio `set-config` assignments:
/// `group.field=value`, the value as the device key's own text — a JSON string unquoted, a
/// number or bool as written. control.radio's key table then parses it against the key's kind.
pub fn config_assignments(config: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    config
        .iter()
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => format!("{k}={s}"),
            other => format!("{k}={other}"),
        })
        .collect()
}

/// Registry fixtures for tests elsewhere in the crate that need a Node on a carrier.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Puts `dev_eui` into this Hub's registry under `root` on `carrier`, the record `node-add`
    /// writes. A Node already there moves: one registry, one carrier per Node.
    pub(crate) fn register(root: &std::path::Path, carrier: Carrier, dev_eui: &str) {
        use control_radio::registry::{self, CarrierRecord, LorawanParams};

        let store = Store::new(root);
        let (radio, lorawan) = match carrier {
            Carrier::P2p => (Transport::P2p, None),
            Carrier::Lorawan => (
                Transport::Lorawan,
                Some(LorawanParams {
                    activation: "otaa".into(),
                    join_eui: None,
                }),
            ),
        };
        registry::apply(
            &store,
            radio,
            &[CarrierRecord {
                dev_eui: dev_eui.to_ascii_lowercase(),
                serial: dev_eui[dev_eui.len().saturating_sub(8)..].to_ascii_lowercase(),
                profile: "default".into(),
                name: None,
                app_key: Some("00".repeat(16)),
                lorawan,
            }],
            &[],
        )
        .expect("register in the registry");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &str = "5876070000000413";

    /// The Nodes list reads each P2P session's home from this Hub's `sessions.db`
    /// (p2p_cluster_gateway.md §5); no file is an empty map, not an error.
    #[test]
    fn p2p_homes_are_read_from_the_sessions_db() {
        use control_radio::p2p::db::SessionsDb;
        let dir = tempfile::tempdir().expect("tempdir");
        let seam = RadioControl::for_tests(Mode::P2p, dir.path());
        assert!(seam.p2p_homes().is_empty());

        let session = |dev_eui: &str, home: Option<&str>| {
            serde_json::from_value::<control_radio::p2p::session::Session>(serde_json::json!({
                "dev_eui": dev_eui,
                "addr": 1,
                "session_key": "00".repeat(16),
                "dev_nonce": 0,
                "last_seen": 0,
                "home": home,
            }))
            .expect("session")
        };
        let mut db = SessionsDb::default();
        db.sessions.insert(DEV.into(), session(DEV, None));
        db.sessions.insert(
            "5876070000000414".into(),
            session("5876070000000414", Some("proximos-2200ad33")),
        );
        db.save(&Store::new(dir.path())).expect("save");

        let homes = seam.p2p_homes();
        assert_eq!(homes.get(DEV), Some(&None), "local");
        assert_eq!(
            homes.get("5876070000000414"),
            Some(&Some("proximos-2200ad33".to_string()))
        );
    }

    /// Decision #23 on one registry: the registry's `radio` is the Node's carrier on every Hub,
    /// a single-carrier one included, and a Node kept on a carrier the mode has switched off is
    /// `disabled`, not moved onto the Hub's carrier (node_registry.md §4).
    #[test]
    fn the_registry_decides_the_carrier_and_the_mode_whether_it_is_on() {
        for (mode, lorawan_on, p2p_on) in [
            (Some(Mode::Lorawan), true, false),
            (Some(Mode::P2p), false, true),
            (Some(Mode::Dual), true, true),
            (Some(Mode::Off), false, false),
            (None, true, true),
        ] {
            for (held, on) in [(Transport::Lorawan, lorawan_on), (Transport::P2p, p2p_on)] {
                let got = resolve_carrier(mode, Some(held));
                let carrier = match held {
                    Transport::P2p => Carrier::P2p,
                    Transport::Lorawan => Carrier::Lorawan,
                };
                let case = format!("{held:?} on {mode:?}");
                assert_eq!(got.carrier, carrier, "{case}");
                assert_eq!(got.registered_on(), Some(carrier), "{case}");
                assert_eq!(got.enabled, on, "{case}");
                assert_eq!(got.disabled(), !on, "{case}");
            }
        }
    }

    /// #470 H4: a Node whose DevEUI's low 32 bits a P2P Node of the registry already holds
    /// collides with it; a LoRaWAN Node at the same address does not, nor does the Node itself.
    #[test]
    fn a_p2p_address_collides_only_with_another_node_on_p2p() {
        let dir = tempfile::tempdir().expect("tempdir");
        test_support::register(dir.path(), Carrier::P2p, "70b3d57ed0000abe");
        test_support::register(dir.path(), Carrier::Lorawan, "11223344d0000abf");
        let seam = RadioControl::for_tests(Mode::Dual, dir.path());
        assert_eq!(
            seam.p2p_address_collision("58760700D0000ABE"),
            Some(AddressCollision {
                addr: "D0000ABE".into(),
                holders: vec!["70b3d57ed0000abe".into()],
            })
        );
        assert_eq!(
            seam.p2p_address_collision("70b3d57ed0000abe"),
            None,
            "itself"
        );
        assert_eq!(
            seam.p2p_address_collision("58760700d0000abf"),
            None,
            "LoRaWAN only"
        );
        assert_eq!(seam.p2p_address_collision("5876070000005722"), None);
        assert_eq!(seam.p2p_address_collision("not-a-eui"), None);
    }

    /// #470 H4: the P2P modulation is the rendered one where the mode runs P2P (`lora`, the
    /// default, with no `p2p_radio` section), and absent where it does not.
    #[test]
    fn the_p2p_modulation_shows_only_where_the_mode_runs_p2p() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (mode, expected) in [
            (Mode::P2p, Some("lora")),
            (Mode::Dual, Some("lora")),
            (Mode::Lorawan, None),
            (Mode::Off, None),
        ] {
            let seam = RadioControl::for_tests(mode, dir.path());
            assert_eq!(seam.p2p_modulation(), expected, "{mode:?}");
        }
        assert_eq!(
            RadioControl::with_rendered_mode(None).p2p_modulation(),
            None
        );
    }

    /// A Node the registry does not hold goes out on the registration default: the Hub's mode,
    /// P2P on a `dual` Hub (Hynek 2026-09-27). It is never `disabled`: nothing holds it.
    #[test]
    fn a_node_the_registry_does_not_hold_resolves_the_registration_default() {
        for (mode, expected) in [
            (Some(Mode::P2p), Carrier::P2p),
            (Some(Mode::Lorawan), Carrier::Lorawan),
            (Some(Mode::Dual), Carrier::P2p),
            (Some(Mode::Off), Carrier::Lorawan),
            (None, Carrier::Lorawan),
        ] {
            let got = resolve_carrier(mode, None);
            assert_eq!(got.carrier, expected, "{mode:?}");
            assert_eq!(got.carrier, registration_default(mode), "{mode:?}");
            assert_eq!(got.registered_on(), None, "{mode:?}");
            assert!(!got.disabled(), "{mode:?}");
        }
    }

    /// [`RadioControl::carrier_for`] against a real registry: a Node is on one carrier, a second
    /// registration moves it, and the DevEUI's case does not matter.
    #[test]
    fn carrier_for_reads_the_one_registry() {
        for mode in [Mode::P2p, Mode::Lorawan, Mode::Dual] {
            let dir = tempfile::tempdir().expect("tempdir");
            let seam = RadioControl::for_tests(mode, dir.path());
            assert_eq!(seam.carrier_for(DEV).registered_on(), None, "{mode:?}");

            test_support::register(dir.path(), Carrier::P2p, DEV);
            let on = seam.carrier_for(DEV);
            assert_eq!(on.registered_on(), Some(Carrier::P2p), "{mode:?}");
            assert_eq!(on.disabled(), mode == Mode::Lorawan, "{mode:?}");

            test_support::register(dir.path(), Carrier::Lorawan, DEV);
            let moved = seam.carrier_for(&DEV.to_ascii_uppercase());
            assert_eq!(moved.registered_on(), Some(Carrier::Lorawan), "{mode:?}");
            assert_eq!(moved.disabled(), mode == Mode::P2p, "{mode:?}");
        }
    }

    /// R6: the bulk move is every Node on the switched-off carrier, to the one the Hub runs, and
    /// is not offered where both run, none runs, or no Node is off.
    #[test]
    fn the_bulk_move_takes_the_nodes_off_the_switched_off_carrier() {
        const LRW: [&str; 2] = ["5876070000000413", "5876070000000414"];
        const P2P: &str = "5876070000005722";
        let dir = tempfile::tempdir().expect("tempdir");
        let seam = |mode| RadioControl::for_tests(mode, dir.path());
        assert_eq!(seam(Mode::P2p).bulk_move(), None, "no registry yet");
        for dev in LRW {
            test_support::register(dir.path(), Carrier::Lorawan, dev);
        }
        test_support::register(dir.path(), Carrier::P2p, P2P);

        assert_eq!(
            seam(Mode::P2p).bulk_move(),
            Some(BulkMove {
                from: Carrier::Lorawan,
                to: Carrier::P2p,
                dev_euis: LRW.map(String::from).to_vec(),
            })
        );
        assert_eq!(
            seam(Mode::Lorawan).bulk_move(),
            Some(BulkMove {
                from: Carrier::P2p,
                to: Carrier::Lorawan,
                dev_euis: vec![P2P.to_string()],
            })
        );
        assert_eq!(seam(Mode::Dual).bulk_move(), None, "both run");
        assert_eq!(seam(Mode::Off).bulk_move(), None, "nothing to move to");
        assert_eq!(
            RadioControl::new(None, None, dir.path().to_string_lossy().into_owned()).bulk_move(),
            None,
            "a seam with no mode"
        );
    }

    /// R8 (§12): a switch asks about the Nodes on the carriers it switches off, offers a move
    /// only to a carrier the new mode runs, and does not ask when nothing goes off.
    #[test]
    fn a_switch_asks_about_the_nodes_on_the_carriers_it_switches_off() {
        const LRW: &str = "5876070000000413";
        const P2P: &str = "5876070000005722";
        let dir = tempfile::tempdir().expect("tempdir");
        let seam = |mode| RadioControl::for_tests(mode, dir.path());
        assert_eq!(
            seam(Mode::Lorawan).node_switch(Mode::P2p),
            None,
            "no registry yet"
        );
        test_support::register(dir.path(), Carrier::Lorawan, LRW);
        test_support::register(dir.path(), Carrier::P2p, P2P);
        let lrw = || (LRW.to_string(), Carrier::Lorawan);
        let p2p = || (P2P.to_string(), Carrier::P2p);

        // lorawan → p2p: the LoRaWAN Nodes, movable to P2P, which the new mode runs.
        assert_eq!(
            seam(Mode::Lorawan).node_switch(Mode::P2p),
            Some(NodeSwitch {
                nodes: vec![lrw()],
                move_to: Some(Carrier::P2p),
            })
        );
        // dual → lorawan: the P2P Nodes, to LoRaWAN.
        assert_eq!(
            seam(Mode::Dual).node_switch(Mode::Lorawan),
            Some(NodeSwitch {
                nodes: vec![p2p()],
                move_to: Some(Carrier::Lorawan),
            })
        );
        // dual → off: both carriers go off, and there is nowhere to move them.
        assert_eq!(
            seam(Mode::Dual).node_switch(Mode::Off),
            Some(NodeSwitch {
                nodes: vec![lrw(), p2p()],
                move_to: None,
            })
        );
        // p2p → off: only P2P goes off; the LoRaWAN Node was already off and is not asked about.
        assert_eq!(
            seam(Mode::P2p).node_switch(Mode::Off),
            Some(NodeSwitch {
                nodes: vec![p2p()],
                move_to: None,
            })
        );
        // Nothing goes off: no question.
        for (from, to) in [
            (Mode::P2p, Mode::Dual),
            (Mode::Lorawan, Mode::Dual),
            (Mode::Off, Mode::P2p),
            (Mode::Off, Mode::Lorawan),
        ] {
            assert_eq!(seam(from).node_switch(to), None, "{from:?} → {to:?}");
        }
        assert_eq!(
            RadioControl::new(None, None, dir.path().to_string_lossy().into_owned())
                .node_switch(Mode::P2p),
            None,
            "a seam with no mode"
        );
    }

    /// A carrier goes off but holds no Node: no question either.
    #[test]
    fn a_switch_with_no_node_on_the_carrier_it_switches_off_does_not_ask() {
        let dir = tempfile::tempdir().expect("tempdir");
        test_support::register(dir.path(), Carrier::P2p, "5876070000005722");
        assert_eq!(
            RadioControl::for_tests(Mode::Lorawan, dir.path()).node_switch(Mode::P2p),
            None
        );
    }

    /// R7: the carrier of a Node the registry keeps on a carrier the mode has switched off.
    #[test]
    fn a_node_on_a_carrier_the_mode_switched_off_is_named() {
        const LRW: &str = "5876070000000413";
        const P2P: &str = "5876070000005722";
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::new(dir.path());
        assert_eq!(
            switched_off(Mode::P2p, &store, LRW),
            None,
            "no registry yet"
        );
        test_support::register(dir.path(), Carrier::Lorawan, LRW);
        test_support::register(dir.path(), Carrier::P2p, P2P);

        assert_eq!(switched_off(Mode::P2p, &store, LRW), Some(Carrier::Lorawan));
        assert_eq!(switched_off(Mode::P2p, &store, P2P), None);
        assert_eq!(switched_off(Mode::Lorawan, &store, P2P), Some(Carrier::P2p));
        assert_eq!(switched_off(Mode::Lorawan, &store, LRW), None);
        assert_eq!(switched_off(Mode::Off, &store, LRW), Some(Carrier::Lorawan));
        assert_eq!(switched_off(Mode::Off, &store, P2P), Some(Carrier::P2p));
        for dev in [LRW, P2P] {
            assert_eq!(
                switched_off(Mode::Dual, &store, dev),
                None,
                "dual runs both"
            );
        }
        assert_eq!(
            switched_off(Mode::Off, &store, "5876070000009999"),
            None,
            "a Node the registry does not hold"
        );
        // Upper case, as a URL may carry it.
        assert_eq!(
            switched_off(Mode::P2p, &store, &LRW.to_ascii_uppercase()),
            Some(Carrier::Lorawan)
        );
        // A seam with no mode says nothing.
        assert_eq!(
            RadioControl::new(None, None, dir.path().to_string_lossy().into_owned())
                .carrier_switched_off(LRW),
            None
        );
    }
}
