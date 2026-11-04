//! The P2P persistent state — the node registry, live sessions, and the `node-get` cache, all
//! in one file (`Slot::P2pSessions` = `p2p/sessions.db`, `store.rs`).
//!
//! Mirrors the LoRaWAN arm's [`crate::lorawan::enroll::NodesFile`]: a single JSON document keyed
//! by `serial`, loaded whole, written back atomically (0600 — it holds `app_key` and the derived
//! `session_key`). Keeping registry + sessions + cache in one slot rather than a per-node file
//! tree preserves the enumerable-file-surface property decision #6 relies on (the Landlock and
//! AppArmor tables name `p2p/sessions.db`, not a glob): `plan/control/radio/plan.md` §5's
//! per-node `cache/{id}.json` layout is folded here for the same reason, and `clear`/`wipe` wipe
//! it by removing the one slot.

use std::collections::BTreeMap;

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::p2p::session::{DownlinkPhase, QueuedDownlink, RegisteredNode, Session};
use crate::store::{Slot, StagedWrite, Store};

/// The last decoded data the central holds for a node, for `node-get` (never triggers a
/// downlink — it is a read of what already arrived, `plan/control/radio/plan.md` §5).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeCache {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reading: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_alarm: Option<serde_json::Value>,
}

/// The operator's `radio-set` override — the **effective** radio parameters, which outrank the
/// rendered Hub config (`radio.p2p_radio`) wherever they are set.
///
/// It lives here rather than in `config.yaml` because decision #4 puts provisioned and role state
/// in the device config and operator state out of it: the Hub config file is rendered, and a
/// runtime lever that rewrote it would be lost on the next render and would make the file two
/// things at once. Only the spreading factor is settable today — the northbridge has one radio,
/// so SF is network-wide by construction, and the nodes discover a changed one themselves.
///
/// `generation` is the whole cross-process protocol: the CLI increments it under the session-DB
/// lock, and the running central compares it against the generation it has applied. It is a
/// change *marker*, not an ordering — `--reset` drops the record and takes it back to 0, which
/// the central reads as "something changed" just the same.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RadioOverride {
    pub spreading_factor: u8,
    pub generation: u64,
    /// RFC 3339, for the operator reading `sessions.db` or a support bundle. Never parsed.
    pub set_at: String,
}

/// How many downlinks may wait for one node before `node-send` refuses to add more.
///
/// A P2P downlink only moves when the node transmits, so an unbounded queue turns a scripting
/// mistake into a backlog the operator has to wait out one uplink at a time. Eight is well past
/// any real command batch (`set-config` is the largest, a frame per key) and small enough to
/// drain in one reporting cycle.
pub const MAX_QUEUED_DOWNLINKS: usize = 8;

/// How many gateway TX counters one `TWR_CTR_BLOCK` hands out (`p2p_tower_gateway.md` §3.4).
pub const COUNTER_BLOCK: u32 = 4096;

/// **Test and bench only**: overrides [`COUNTER_BLOCK`] so a HIL run sees the gateway use a
/// block up, ask for the next (`EVT_TWR_CTR_LOW`) and get it (#470 HIL T5) in minutes instead
/// of thousands of frames. Read once at start; an integer in [`COUNTER_BLOCK_OVERRIDE_MIN`]
/// ..= [`COUNTER_BLOCK`]. Unset keeps the default; anything else is ignored, said once.
///
/// A small block costs nothing in safety — counters are never reused, only issued in smaller
/// steps — but it costs a `sessions.db` write per block, which is why it stays a debug knob.
pub const COUNTER_BLOCK_ENV: &str = "PROXIMOS_P2P_COUNTER_BLOCK";

/// The smallest block [`COUNTER_BLOCK_ENV`] may ask for: the gateway asks for the next one
/// below a quarter left, so this leaves it two counters of headroom.
pub const COUNTER_BLOCK_OVERRIDE_MIN: u32 = 8;

/// [`COUNTER_BLOCK_ENV`]'s value as the override it asks for, or why it is ignored.
pub fn parse_counter_block_override(value: &str) -> std::result::Result<u32, String> {
    let n: u32 = value
        .trim()
        .parse()
        .map_err(|_| format!("{value:?} is not a whole number"))?;
    if (COUNTER_BLOCK_OVERRIDE_MIN..=COUNTER_BLOCK).contains(&n) {
        Ok(n)
    } else {
        Err(format!(
            "{n} is outside {COUNTER_BLOCK_OVERRIDE_MIN}..={COUNTER_BLOCK}"
        ))
    }
}

/// The block size in force: [`COUNTER_BLOCK_ENV`]'s override, else [`COUNTER_BLOCK`]. Read
/// once per process; an override in force is said loudly on stderr the first time, and again
/// by the central on every gateway restore ([`counter_block_override`]).
pub fn counter_block() -> u32 {
    counter_block_override().unwrap_or(COUNTER_BLOCK)
}

/// The block-size override in force, if any (see [`COUNTER_BLOCK_ENV`]).
pub fn counter_block_override() -> Option<u32> {
    static OVERRIDE: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        let value = std::env::var(COUNTER_BLOCK_ENV).ok()?;
        match parse_counter_block_override(&value) {
            Ok(n) => {
                eprintln!(
                    "[p2p] WARNING: {COUNTER_BLOCK_ENV}={n} — TEST/BENCH ONLY: gateway counter \
                     blocks of {n}, not {COUNTER_BLOCK}. Remove it outside a HIL run."
                );
                Some(n)
            }
            Err(why) => {
                eprintln!(
                    "[p2p] WARNING: {COUNTER_BLOCK_ENV} ignored ({why}); counter blocks stay \
                     {COUNTER_BLOCK}"
                );
                None
            }
        }
    })
}

/// The highest counter a block may end on: `last < 2³² − 1` (§2.1).
pub const COUNTER_MAX: u32 = u32::MAX - 1;

/// The whole P2P state document.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionsDb {
    /// Registered nodes, keyed by `dev_eui` — the join admission gate (`p2p.md` §7/§8,
    /// re-keyed by #73). The serial survives inside the record as a registration attribute.
    #[serde(default)]
    pub nodes: BTreeMap<String, RegisteredNode>,
    /// Join sessions, keyed by `dev_eui`: live ones, and parked ones whose Node has moved to
    /// another carrier ([`SessionsDb::is_parked`]).
    #[serde(default)]
    pub sessions: BTreeMap<String, Session>,
    /// **Deprecated, and no longer written.** `node-get`'s P2P answers moved to the shared
    /// per-transport [`crate::cache::Cache`] (`p2p/cache/{dev_eui}.json`) under decision #4, so
    /// both arms return one document shape and one slot vocabulary. The field survives with
    /// `#[serde(default)]` only so a bench database written before that still loads — nothing
    /// reads it, and `remove_node` clears a legacy entry rather than leaving it behind.
    #[deprecated(note = "use crate::cache::Cache::new(store, Transport::P2p)")]
    #[serde(default)]
    pub cache: BTreeMap<String, NodeCache>,
    /// The operator's `radio-set` override, or `None` when the Hub config's values stand. Absent
    /// from every database written before `radio-set` existed, hence `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radio: Option<RadioOverride>,
    /// The last gateway TX counter any issued `TWR_CTR_BLOCK` covers, 0 before the first. The
    /// gateway seals its ACKs and downlinks under `net_id ‖ counter`, so a counter must never be
    /// handed out twice while `net_id` stands: the central writes this **before** it sends a
    /// block, and the next block starts after it (`p2p_tower_gateway.md` §3.4). `clear` keeps
    /// it; `wipe`, which also replaces `net_id`, drops it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ctr_block_end: u32,
    /// The `nodes` [`SessionsDb::load`] took from the registry, so [`SessionsDb::save`] writes
    /// back only what this copy changed.
    #[serde(skip)]
    registry_base: crate::registry::ViewBase<BTreeMap<String, RegisteredNode>>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Does this document hold sessions of the earlier P2P wire — any session without an `addr`?
///
/// Checked only once the typed parse has already failed, as [`is_pre_dev_eui`] is. The earlier
/// wire's sessions carry a 16-bit `dev_addr`, an `uplink_counter` and a `state`, and their keys
/// were derived under a different construction, so none of them can be carried over: the
/// sessions are dropped and the Nodes join again (flag day, `p2p_tower_gateway.md` §1).
fn has_earlier_wire_sessions(raw: &serde_json::Value) -> bool {
    raw.get("sessions")
        .and_then(|s| s.as_object())
        .is_some_and(|m| m.values().any(|s| s.get("addr").is_none()))
}

/// Does this document have the pre-#73 shape — a session with a `serial` and no `dev_eui`?
///
/// Checked only once the typed parse has already failed, so the happy path is untouched. It
/// exists to turn serde's generic "missing field `dev_eui`" into the one thing the operator
/// actually has to do.
fn is_pre_dev_eui(raw: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    let Some(sessions) = v.get("sessions").and_then(|s| s.as_object()) else {
        return false;
    };
    sessions
        .values()
        .any(|s| s.get("dev_eui").is_none() && s.get("serial").is_some())
}

/// The one refusal a pre-#73 database gets, however it was spotted. There is no migration on
/// purpose: every session in such a document holds a key derived under the old serial-keyed
/// KDF, so a migrated session would decrypt nothing — and a registry whose keys were rewritten
/// underneath the operator would still be one nobody had checked.
fn pre_dev_eui_err(store: &Store, how: &str) -> Error {
    let path = store.path(Slot::P2pSessions);
    db_err(format!(
        "is from before the DevEUI identity change (#73): {how}. There is no migration — \
         delete it and re-run node-add: rm {} {}/cache/*.json",
        path.display(),
        path.parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    ))
}

fn db_err(why: impl std::fmt::Display) -> Error {
    Error::new(
        Kind::StorageError,
        Transport::P2p,
        Component::SessionDb,
        format!("p2p/sessions.db {why}"),
    )
}

impl SessionsDb {
    /// Load, treating absent or empty as a fresh (empty) database.
    ///
    /// `nodes` is the registry's P2P view (node_registry.md, R2): the Nodes whose `radio` is
    /// `p2p`, whatever the file's own `nodes` map says. That map is only a mirror a rolled-back
    /// build reads.
    pub fn load(store: &Store) -> Result<Self> {
        let mut db = Self::load_file(store)?;
        db.nodes = crate::registry::current(store)?
            .on(Transport::P2p)
            .filter_map(|n| {
                let registered = n.as_registered();
                if registered.is_none() {
                    eprintln!(
                        "[registry] {} is on p2p without an app_key; not admitted",
                        n.dev_eui
                    );
                }
                registered
            })
            .map(|n| (n.dev_eui.clone(), n))
            .collect();
        db.registry_base = crate::registry::ViewBase::new(db.nodes.clone());
        Ok(db)
    }

    /// The file alone, as a pre-registry build wrote it — what the migration merges.
    pub fn load_file(store: &Store) -> Result<Self> {
        match store.read(Slot::P2pSessions)? {
            None => Ok(Self::default()),
            Some(raw) if raw.trim().is_empty() => Ok(Self::default()),
            Some(raw) => {
                let db: SessionsDb = match serde_json::from_str(&raw) {
                    Ok(db) => db,
                    Err(_) if is_pre_dev_eui(&raw) => {
                        return Err(pre_dev_eui_err(
                            store,
                            "its sessions are keyed by serial and carry no dev_eui",
                        ))
                    }
                    Err(e) => Self::without_earlier_wire_sessions(&raw)
                        .ok_or_else(|| db_err(format!("is not readable: {e}")))?,
                };
                // Both maps are keyed by `dev_eui` since #73, and each record repeats it — so a
                // key that is not its own record's `dev_eui` is a pre-#73 document that happened
                // to deserialize. One does, whenever there are no live sessions:
                // `RegisteredNode` is byte-for-byte unchanged by #73 (it always carried both
                // fields), so the missing-field signal above never fires on a registry-only
                // database, and the registry would silently stay serial-keyed.
                if let Some((key, node)) = db.nodes.iter().find(|(k, n)| **k != n.dev_eui) {
                    return Err(pre_dev_eui_err(
                        store,
                        &format!(
                            "its node registry is keyed by {key:?}, not by that record's \
                             dev_eui {:?}",
                            node.dev_eui
                        ),
                    ));
                }
                if let Some((key, session)) = db.sessions.iter().find(|(k, s)| **k != s.dev_eui) {
                    return Err(pre_dev_eui_err(
                        store,
                        &format!(
                            "its session map is keyed by {key:?}, not by that session's \
                             dev_eui {:?}",
                            session.dev_eui
                        ),
                    ));
                }
                Ok(db)
            }
        }
    }

    /// The document with its sessions of the earlier P2P wire dropped, or `None` when that is
    /// not what made it unreadable. Everything else stays: the registry mirror, the radio
    /// override. Said once per process; the central's next save writes the document without
    /// them, so the line stops there.
    fn without_earlier_wire_sessions(raw: &str) -> Option<Self> {
        let mut v: serde_json::Value = serde_json::from_str(raw).ok()?;
        if !has_earlier_wire_sessions(&v) {
            return None;
        }
        let dropped = v
            .get("sessions")
            .and_then(|s| s.as_object())
            .map_or(0, |m| m.len());
        if let Some(map) = v.as_object_mut() {
            map.insert(
                "sessions".into(),
                serde_json::Value::Object(Default::default()),
            );
        }
        let db: SessionsDb = serde_json::from_value(v).ok()?;
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            eprintln!(
                "[p2p] p2p/sessions.db holds {dropped} session(s) of the earlier P2P radio link; \
                 they are dropped and the Nodes join again"
            );
        });
        Some(db)
    }

    /// Save: this copy's changes to `nodes` go to the registry first, then the file, whose
    /// `nodes` map is the mirror. A failed file write leaves the registry ahead, which the next
    /// `load` reads.
    pub fn save(&self, store: &Store) -> Result<()> {
        self.stage_save(store)?.commit()
    }

    /// [`Self::save`] up to the file's temp copy: the registry is written, the file is staged
    /// ([`Store::stage`]) for the caller to flush and commit alongside another write (#130).
    pub fn stage_save<'s>(&self, store: &'s Store) -> Result<StagedWrite<'s>> {
        let (upserts, removals) =
            crate::registry::diff(self.registry_base.get().as_ref(), &self.nodes, |r| {
                crate::registry::CarrierRecord::from(r)
            });
        crate::registry::apply(store, Transport::P2p, &upserts, &removals)?;
        self.registry_base.set(self.nodes.clone());
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| db_err(format!("could not be serialized: {e}")))?;
        store.stage(Slot::P2pSessions, json.as_bytes())
    }

    /// Write a node-registry record, keyed by `dev_eui`. Low-level: the `node-add` dispatch
    /// path guards existence first and refuses a duplicate with `AlreadyExists`
    /// (`component: node-registry`) — registration is add-only, and an `app_key` rotation is a
    /// `node-remove` + `node-add` (`p2p.md` §10, decided 2026-08-31) — so in practice this only
    /// ever inserts a fresh node. The bare overwrite is kept for the engine tests that stage a
    /// registry directly.
    pub fn upsert_node(&mut self, node: RegisteredNode) {
        self.nodes.insert(node.dev_eui.clone(), node);
    }

    /// The registry lookup the join path takes — a direct hit on the map key (#73).
    pub fn node(&self, dev_eui: &str) -> Option<&RegisteredNode> {
        self.nodes.get(dev_eui)
    }

    /// The same lookup by the *attribute*, which costs a scan. Kept under its honest name for
    /// `--serial`, the operator-facing alias: the serial is printed on the device, so it stays
    /// a way in even though it is no longer an index.
    pub fn node_by_serial(&self, serial: &str) -> Option<&RegisteredNode> {
        self.nodes.values().find(|n| n.serial == serial)
    }

    /// The session on the air address `addr`, parked or live. `node-add` and `node-move`
    /// refuse a second P2P Node on one address, so there is at most one outside a registry
    /// written around that check; the central then serves neither ([`Self::nodes_on_addr`]).
    pub fn session_by_addr(&self, addr: u32) -> Option<&Session> {
        self.sessions.values().find(|s| s.addr == addr)
    }

    pub fn session_by_addr_mut(&mut self, addr: u32) -> Option<&mut Session> {
        self.sessions.values_mut().find(|s| s.addr == addr)
    }

    /// The Nodes on the P2P view whose DevEUI maps to `addr`, in DevEUI order.
    pub fn nodes_on_addr(&self, addr: u32) -> Vec<&RegisteredNode> {
        self.nodes
            .values()
            .filter(|n| crate::p2p::tower::addr_of(&n.dev_eui) == Some(addr))
            .collect()
    }

    /// Reserve the next gateway TX counter block, `(first, last)` inclusive, and record its end
    /// in [`Self::ctr_block_end`]. The caller saves the document **before** it sends the block.
    ///
    /// Exhaustion is a storage error rather than a wrap: a wrapped counter repeats a nonce. It
    /// is 2³² / 4096 ≈ a million blocks away, and `wipe` (a new `net_id`) is the way back.
    ///
    /// `gateway_floor` is the highest counter the running gateway may already have spent (its
    /// `GET_INFO` `ctr_last`, or `ctr_next − 1`): the Northbridge never goes back below its
    /// next counter and refuses a block that ends under it (`BAD_PARAM`), so a block starts
    /// above both the stored end and that floor. It only matters when the file was replaced
    /// while the gateway kept running.
    pub fn reserve_counter_block(&mut self, gateway_floor: u32) -> Result<(u32, u32)> {
        self.reserve_counter_block_of(gateway_floor, counter_block())
    }

    /// [`Self::reserve_counter_block`] of `size` counters (at least one).
    pub fn reserve_counter_block_of(
        &mut self,
        gateway_floor: u32,
        size: u32,
    ) -> Result<(u32, u32)> {
        let first = self
            .ctr_block_end
            .max(gateway_floor)
            .checked_add(1)
            .filter(|f| *f <= COUNTER_MAX);
        let Some(first) = first else {
            return Err(db_err(
                "has issued every gateway TX counter under this net_id; `wipe --force` and \
                 `create` issue a new net_id",
            ));
        };
        let last = first.saturating_add(size.max(1) - 1).min(COUNTER_MAX);
        self.ctr_block_end = last;
        Ok((first, last))
    }

    /// `clear`: every Node, session and override goes, the counter floor stays — `net_id` stays
    /// with it, and a counter reused under the same `net_id` would repeat a gateway nonce. The
    /// file is removed outright when no block was ever issued. The caller holds the lock.
    pub fn clear_keeping_counter(store: &Store) -> Result<()> {
        let end = Self::load_file(store)?.ctr_block_end;
        if end == 0 {
            return store.remove(Slot::P2pSessions);
        }
        let json = serde_json::to_string_pretty(&serde_json::json!({ "ctr_block_end": end }))
            .map_err(|e| db_err(format!("could not be serialized: {e}")))?;
        store.write(Slot::P2pSessions, &json)
    }

    /// Append a downlink to a node's queue (S2 / FW B4 — what `node-send` writes), returning the
    /// new queue depth. `None` when the node has no live session: the gateway seals a P2P
    /// downlink under the session key, so there is nothing to queue it against until the node
    /// has joined.
    pub fn enqueue_downlink(&mut self, dev_eui: &str, entry: QueuedDownlink) -> Option<usize> {
        let s = self.sessions.get_mut(dev_eui)?;
        s.downlink_queue.push_back(entry);
        Some(s.downlink_queue.len())
    }

    /// Take the downlinks queued under `seqs` back out of a node's queue, keeping the rest in
    /// order, and return how many went: the Portal's discard of a queued draft (P-6), which on
    /// LoRaWAN takes the frames out of ChirpStack's queue.
    ///
    /// A head already delivered (`Sent`, awaiting the node's `0x55`) stays: the node may have
    /// applied it, and dropping it would only lose the answer. An announced head may go; the
    /// phase returns to `Idle`, so the next entry is announced with its own length.
    pub fn drop_queued_seqs(&mut self, dev_eui: &str, seqs: &[u32]) -> usize {
        let Some(s) = self.sessions.get_mut(dev_eui) else {
            return 0;
        };
        let keep_head = s.downlink_phase == crate::p2p::session::DownlinkPhase::Sent;
        let before = s.downlink_queue.len();
        let mut index = 0usize;
        let mut head_dropped = false;
        s.downlink_queue.retain(|entry| {
            let is_head = index == 0;
            index += 1;
            let drop = entry.seq.is_some_and(|q| seqs.contains(&q)) && !(is_head && keep_head);
            if drop && is_head {
                head_dropped = true;
            }
            !drop
        });
        if head_dropped {
            s.downlink_phase = crate::p2p::session::DownlinkPhase::Idle;
        }
        before - s.downlink_queue.len()
    }

    /// A Discard's withdrawal (PF-3b, nodes_phase1.cs.md dodatek 2026-09-27): as
    /// [`Self::drop_queued_seqs`], but a head already delivered (`Sent`) goes too, and the phase
    /// returns to `Idle`. The Hub cannot know whether the node took that `0x56` — on the bench a
    /// muted node "received" it into a deaf RX1 — and re-delivering a discarded `SetParam`
    /// three uplinks later is always wrong. Only the Discard uses this; the history retry and the
    /// portal's resend keep a delivered head, whose late answer still counts.
    ///
    /// Returns how many entries went and whether one of them was a delivered head, which the
    /// node may have applied.
    pub fn withdraw_seqs(&mut self, dev_eui: &str, seqs: &[u32]) -> (usize, bool) {
        let Some(s) = self.sessions.get_mut(dev_eui) else {
            return (0, false);
        };
        let head_sent = s.downlink_phase == crate::p2p::session::DownlinkPhase::Sent;
        let before = s.downlink_queue.len();
        let mut index = 0usize;
        let mut head_dropped = false;
        s.downlink_queue.retain(|entry| {
            let is_head = index == 0;
            index += 1;
            let drop = entry.seq.is_some_and(|q| seqs.contains(&q));
            if drop && is_head {
                head_dropped = true;
            }
            !drop
        });
        if head_dropped {
            s.downlink_phase = crate::p2p::session::DownlinkPhase::Idle;
        }
        (before - s.downlink_queue.len(), head_dropped && head_sent)
    }

    /// The queued downlinks that have not gone out yet, `(seq, body)` in queue order: every entry
    /// carrying a `seq` but a head already delivered (`Sent`, awaiting the node's `0x55`), which
    /// is on the air. What coalescing may withdraw ([`crate::coalesce`]), through
    /// [`Self::drop_queued_seqs`], which keeps that head too. Empty with no live session.
    pub fn waiting_downlinks(&self, dev_eui: &str) -> Vec<(u32, &[u8])> {
        let Some(s) = self.sessions.get(dev_eui) else {
            return Vec::new();
        };
        let delivered = usize::from(s.downlink_phase == crate::p2p::session::DownlinkPhase::Sent);
        s.downlink_queue
            .iter()
            .skip(delivered)
            .filter_map(|q| Some((q.seq?, q.body.as_slice())))
            .collect()
    }

    /// How many downlinks are already waiting for a node, or `None` with no live session.
    pub fn queued_downlinks(&self, dev_eui: &str) -> Option<usize> {
        Some(self.sessions.get(dev_eui)?.downlink_queue.len())
    }

    /// Park a Node's session at a move off P2P (node_registry.md §4, amended 2026-09-28, Q-N4)
    /// and return whether there was one.
    ///
    /// The session stays because the Node keeps it in NVS: moved back, its next frame under the
    /// old session is understood at once, instead of being ignored until its link check gives
    /// up and it rejoins (bench A4-2, about 5.5 minutes). The central takes it off the gateway
    /// while it is parked and gives it back, with its `last_seen`, on the move back. What goes
    /// is the state of the P2P link's downlinks: the queue, the dispatch phase, and a history
    /// replay in progress. None of it is carried to the new carrier, and none of it may reach
    /// the Node when it comes back. The key, both counters and `addr` are the session's own and
    /// stay.
    pub fn park_session(&mut self, dev_eui: &str) -> bool {
        let Some(s) = self.sessions.get_mut(dev_eui) else {
            return false;
        };
        s.downlink_queue.clear();
        s.downlink_phase = DownlinkPhase::Idle;
        s.history = None;
        true
    }

    /// Whether the session of `dev_eui` is parked: held here while its Node is not on the
    /// registry's P2P view. The gateway does not hold it, and the central answers nothing
    /// under it.
    pub fn is_parked(&self, dev_eui: &str) -> bool {
        self.sessions.contains_key(dev_eui) && !self.nodes.contains_key(dev_eui)
    }

    /// Drop the parked sessions of `dev_euis`, Nodes leaving the registry from another carrier
    /// (`node-remove`, `clear`): §4 drops a Node's session, parked or live. Returns how many
    /// went. The file is written only when one did, so a Hub that never ran P2P gets no
    /// `sessions.db` from a LoRaWAN removal.
    pub fn drop_parked_sessions(store: &Store, dev_euis: &[String]) -> Result<usize> {
        if dev_euis.is_empty() || !store.exists(Slot::P2pSessions) {
            return Ok(0);
        }
        let _db_lock = store.lock(Slot::P2pSessions)?;
        let mut db = Self::load(store)?;
        let parked: Vec<&String> = dev_euis.iter().filter(|id| db.is_parked(id)).collect();
        for id in &parked {
            db.sessions.remove(id.as_str());
        }
        if !parked.is_empty() {
            db.save(store)?;
        }
        Ok(parked.len())
    }

    /// Whether `dev_eui` holds a parked session, for `node-info` on the Node's own carrier. A
    /// degraded read: no `sessions.db`, or one that cannot be read, is "not parked", so a
    /// LoRaWAN `node-info` never fails over the P2P file.
    pub fn has_parked_session(store: &Store, dev_eui: &str) -> bool {
        store.exists(Slot::P2pSessions) && Self::load(store).is_ok_and(|db| db.is_parked(dev_eui))
    }

    /// Remove a node from the registry and return its session, if any. The session itself
    /// stays in [`Self::sessions`]; `node-remove` drops it, the central takes it off the
    /// gateway on its next pass.
    ///
    /// The shared `p2p/cache/` document is the dispatch arm's to remove — it is not in this
    /// file. The legacy in-document [`Self::cache`] entry is cleared here so a database written
    /// before decision #4 does not keep a removed node's readings forever.
    pub fn remove_node(&mut self, dev_eui: &str) -> (Option<RegisteredNode>, Option<Session>) {
        let node = self.nodes.remove(dev_eui);
        let session = self.sessions.get(dev_eui).cloned();
        #[allow(deprecated)]
        self.cache.remove(dev_eui);
        (node, session)
    }
}

/// [`SessionsDb::drop_parked_sessions`] for the removal of Nodes that are not on P2P: a failure
/// is logged, not returned. The Node's own carrier is what the command removes it from, and an
/// unreadable `sessions.db` must not make that fail. A session left behind stays parked, so it
/// is never answered, and a later `node-add --radio p2p` of the DevEUI replaces it.
pub fn drop_parked_or_log(store: &Store, dev_euis: &[String]) {
    match SessionsDb::drop_parked_sessions(store, dev_euis) {
        Ok(0) => {}
        Ok(n) => eprintln!("[registry] dropped {n} parked P2P session(s) with the Node(s)"),
        Err(e) => eprintln!(
            "[registry] the parked P2P session(s) of {} could not be dropped (continuing): {}",
            dev_euis.join(", "),
            e.message()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p2p::session::QueuedDownlink;

    fn node(dev_eui: &str, key: &str) -> RegisteredNode {
        RegisteredNode {
            serial: "80e007b2".into(),
            dev_eui: dev_eui.into(),
            profile: "default".into(),
            app_key: key.into(),
            name: None,
        }
    }

    fn session(dev_eui: &str, addr: u32) -> Session {
        Session {
            dev_eui: dev_eui.into(),
            addr,
            session_key: "00112233445566778899aabbccddeeff".into(),
            dev_nonce: 7,
            last_seen: 0,
            joined_at: None,
            proto_version: 0,
            capabilities: None,
            hello: None,
            downlink_queue: Default::default(),
            downlink_phase: Default::default(),
            history: None,
            detach: None,
            home: None,
        }
    }

    /// P-6 on P2P: a discarded draft leaves the queue, the rest keeps its order, and a head
    /// the node already has is not taken back.
    #[test]
    fn queued_downlinks_are_dropped_by_seq_except_a_delivered_head() {
        use crate::p2p::session::DownlinkPhase;
        let entry = |seq| QueuedDownlink::with_seq(seq, vec![seq as u8]);
        let mut db = SessionsDb::default();
        let mut s = session("5876070000000413", 1);
        s.downlink_queue = [entry(4), entry(5), entry(6)].into_iter().collect();
        s.downlink_phase = DownlinkPhase::Announced;
        db.sessions.insert(s.dev_eui.clone(), s);

        assert_eq!(db.drop_queued_seqs("5876070000000413", &[4, 6]), 2);
        let s = &db.sessions["5876070000000413"];
        assert_eq!(
            s.downlink_queue.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [Some(5)]
        );
        assert_eq!(
            s.downlink_phase,
            DownlinkPhase::Idle,
            "the next head is announced afresh"
        );

        let s = db.sessions.get_mut("5876070000000413").unwrap();
        s.downlink_phase = DownlinkPhase::Sent;
        assert_eq!(
            db.drop_queued_seqs("5876070000000413", &[5]),
            0,
            "already delivered"
        );
        assert_eq!(
            db.drop_queued_seqs("0000000000000000", &[5]),
            0,
            "no session"
        );
    }

    /// Q-N1: what coalescing may withdraw is every queued entry with a `seq` but a delivered
    /// head, which is on the air; an announced head has not gone out yet.
    #[test]
    fn the_waiting_downlinks_leave_out_a_delivered_head() {
        use crate::p2p::session::DownlinkPhase;
        let entry = |seq| QueuedDownlink::with_seq(seq, vec![seq as u8]);
        let mut db = SessionsDb::default();
        let mut s = session("5876070000000413", 1);
        s.downlink_queue = [entry(4), QueuedDownlink::raw(vec![0x11]), entry(6)]
            .into_iter()
            .collect();
        s.downlink_phase = DownlinkPhase::Announced;
        db.sessions.insert(s.dev_eui.clone(), s);
        let seqs = |db: &SessionsDb| -> Vec<u32> {
            db.waiting_downlinks("5876070000000413")
                .into_iter()
                .map(|(seq, _)| seq)
                .collect()
        };
        assert_eq!(seqs(&db), [4, 6], "a raw --hex has no seq to withdraw by");

        db.sessions
            .get_mut("5876070000000413")
            .unwrap()
            .downlink_phase = DownlinkPhase::Sent;
        assert_eq!(seqs(&db), [6], "the delivered head stays out");
        assert!(db.waiting_downlinks("0000000000000000").is_empty());
    }

    /// PF-3b: a Discard takes a delivered head too and says so; the next head is announced
    /// afresh. The ordinary drop above still keeps it.
    #[test]
    fn a_discard_withdraws_a_delivered_head_and_says_the_node_may_have_it() {
        use crate::p2p::session::DownlinkPhase;
        let entry = |seq| QueuedDownlink::with_seq(seq, vec![seq as u8]);
        let mut db = SessionsDb::default();
        let mut s = session("5876070000000413", 1);
        s.downlink_queue = [entry(7), entry(8)].into_iter().collect();
        s.downlink_phase = DownlinkPhase::Sent;
        db.sessions.insert(s.dev_eui.clone(), s);

        assert_eq!(
            db.drop_queued_seqs("5876070000000413", &[7]),
            0,
            "the history retry and the resend keep it"
        );
        assert_eq!(db.withdraw_seqs("5876070000000413", &[7]), (1, true));
        let s = &db.sessions["5876070000000413"];
        assert_eq!(
            s.downlink_queue.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [Some(8)]
        );
        assert_eq!(s.downlink_phase, DownlinkPhase::Idle);
        assert_eq!(
            db.withdraw_seqs("5876070000000413", &[8]),
            (1, false),
            "an undelivered head is simply gone"
        );
        assert_eq!(db.withdraw_seqs("0000000000000000", &[8]), (0, false));
    }

    #[test]
    fn a_fresh_store_loads_an_empty_db() {
        let tmp = std::env::temp_dir().join(format!("p2pdb-empty-{}", std::process::id()));
        let store = Store::new(&tmp);
        let db = SessionsDb::load(&store).expect("empty load");
        assert!(db.nodes.is_empty() && db.sessions.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Every database written before `radio-set` existed — which is every bench database — has no
    /// `radio` key. It must load as "no override", so the Hub config's spreading factor stands,
    /// rather than failing the whole document and taking the registry and sessions with it.
    #[test]
    fn a_db_without_a_radio_override_loads_as_none() {
        let tmp = std::env::temp_dir().join(format!("p2pdb-noradio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = Store::new(&tmp);
        store
            .write(Slot::P2pSessions, r#"{"nodes": {}, "sessions": {}}"#)
            .expect("seed a pre-radio-set document");
        let db = SessionsDb::load(&store).expect("a pre-radio-set document still loads");
        assert_eq!(db.radio, None, "no key means no override");

        // And a set one round-trips whole.
        let mut db = db;
        db.radio = Some(RadioOverride {
            spreading_factor: 9,
            generation: 3,
            set_at: "2026-09-15T10:00:00Z".into(),
        });
        db.save(&store).expect("save");
        let back = SessionsDb::load(&store).expect("reload");
        assert_eq!(back.radio, db.radio);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The flag day (`p2p_tower_gateway.md` §1): a document holding sessions of the earlier
    /// P2P wire loads without them, so the Nodes join again, and keeps everything else — the
    /// registry mirror and the operator's radio override. Saved, it is in the new shape.
    #[test]
    fn sessions_of_the_earlier_wire_are_dropped_and_the_rest_loads() {
        let tmp = std::env::temp_dir().join(format!("p2pdb-earlier-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = Store::new(&tmp);
        // What the earlier central wrote: a 16-bit `dev_addr`, an `uplink_counter`, a state,
        // and a queue from before `seq`.
        let earlier = r#"{
          "nodes": {},
          "sessions": {
            "70b3d57ed0000abe": {
              "dev_eui": "70b3d57ed0000abe",
              "dev_addr": 1,
              "session_key": "00112233445566778899aabbccddeeff",
              "last_join_at": null,
              "dev_nonce": 7,
              "uplink_counter": null,
              "state": "joined",
              "proto_version": 0,
              "downlink_queue": [[17, 34], [51]],
              "downlink_phase": "sent"
            }
          },
          "cache": {},
          "radio": {"spreading_factor": 9, "generation": 2, "set_at": "2026-09-27T10:00:00Z"}
        }"#;
        store.write(Slot::P2pSessions, earlier).expect("seed");

        let db = SessionsDb::load(&store).expect("an earlier document still loads");
        assert!(
            db.sessions.is_empty(),
            "its sessions cannot be carried over"
        );
        assert_eq!(db.radio.as_ref().map(|r| r.spreading_factor), Some(9));
        assert_eq!(db.ctr_block_end, 0, "no block was ever issued");

        db.save(&store).expect("save");
        let raw = store
            .read(Slot::P2pSessions)
            .expect("read")
            .expect("written");
        assert!(!raw.contains("dev_addr"), "{raw}");
        let again = SessionsDb::load(&store).expect("reload");
        assert_eq!(again.radio, db.radio);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A document that is unreadable for any other reason is still refused, not emptied.
    #[test]
    fn a_broken_document_is_refused_rather_than_emptied() {
        let tmp = std::env::temp_dir().join(format!("p2pdb-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = Store::new(&tmp);
        store
            .write(
                Slot::P2pSessions,
                r#"{"sessions": {}, "ctr_block_end": "x"}"#,
            )
            .expect("seed");
        let err = SessionsDb::load(&store).expect_err("refused");
        assert_eq!(err.kind(), Kind::StorageError);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Counter blocks are 4096 wide, the first starts at 1, each starts after the last one's
    /// recorded end, and the space ends short of `u32::MAX` instead of wrapping (§2.1, §3.4).
    #[test]
    fn counter_blocks_follow_each_other_and_never_wrap() {
        let mut db = SessionsDb::default();
        assert_eq!(db.reserve_counter_block(0).expect("first"), (1, 4096));
        assert_eq!(db.ctr_block_end, 4096);
        assert_eq!(db.reserve_counter_block(0).expect("second"), (4097, 8192));

        db.ctr_block_end = COUNTER_MAX - 10;
        assert_eq!(
            db.reserve_counter_block(0).expect("a short last block"),
            (COUNTER_MAX - 9, COUNTER_MAX)
        );
        let err = db.reserve_counter_block(0).expect_err("exhausted");
        assert_eq!(err.kind(), Kind::StorageError);
        assert!(err.message().contains("wipe"), "{}", err.message());
        assert_eq!(db.ctr_block_end, COUNTER_MAX, "a refusal changes nothing");
    }

    /// A gateway that kept running past a replaced file: the block starts above what it may
    /// already have spent, so 0.3.0 never refuses it for ending below its next counter.
    #[test]
    fn a_counter_block_starts_above_the_gateway_floor() {
        let mut db = SessionsDb::default();
        assert_eq!(db.reserve_counter_block(5000).expect("above"), (5001, 9096));
        assert_eq!(db.ctr_block_end, 9096);
        assert_eq!(
            db.reserve_counter_block(100)
                .expect("a lower floor is ignored"),
            (9097, 13192)
        );
    }

    /// The HIL T5 knob: a small block is issued in the same sequence, never overlapping.
    #[test]
    fn a_small_test_block_is_issued_in_sequence() {
        let mut db = SessionsDb::default();
        assert_eq!(db.reserve_counter_block_of(0, 16).expect("first"), (1, 16));
        assert_eq!(
            db.reserve_counter_block_of(0, 16).expect("second"),
            (17, 32)
        );
        assert_eq!(
            db.reserve_counter_block_of(30, 4096)
                .expect("default again"),
            (33, 4128)
        );
    }

    #[test]
    fn the_block_size_override_takes_only_a_sane_test_size() {
        assert_eq!(parse_counter_block_override("64"), Ok(64));
        assert_eq!(parse_counter_block_override(" 8\n"), Ok(8));
        assert_eq!(parse_counter_block_override("4096"), Ok(COUNTER_BLOCK));
        for bad in ["7", "0", "4097", "-1", "lots", ""] {
            assert!(parse_counter_block_override(bad).is_err(), "{bad:?}");
        }
        // Not set in the test environment: the default stands.
        if std::env::var_os(COUNTER_BLOCK_ENV).is_none() {
            assert_eq!(counter_block(), COUNTER_BLOCK);
        }
    }

    /// `clear` keeps the counter floor and nothing else; with no block ever issued the file
    /// goes, as before.
    #[test]
    fn clear_keeps_only_the_counter_floor() {
        let tmp = std::env::temp_dir().join(format!("p2pdb-clear-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = Store::new(&tmp);
        let mut db = SessionsDb::default();
        db.sessions
            .insert("70b3d57ed0000001".into(), session("70b3d57ed0000001", 1));
        db.reserve_counter_block(0).expect("block");
        db.save(&store).expect("save");

        SessionsDb::clear_keeping_counter(&store).expect("clear");
        let back = SessionsDb::load(&store).expect("load");
        assert!(back.sessions.is_empty() && back.radio.is_none());
        assert_eq!(back.ctr_block_end, 4096);

        store
            .write(Slot::P2pSessions, r#"{"sessions": {}}"#)
            .expect("seed");
        SessionsDb::clear_keeping_counter(&store).expect("clear");
        assert!(!store.exists(Slot::P2pSessions));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The one migration that is deliberately refused. A pre-#73 database keys its sessions
    /// by serial and carries no `dev_eui`; loading it would be worse than failing, because
    /// every session in it holds a key derived under the old serial-keyed KDF. A
    /// `#[serde(alias = "serial")]` would be the tempting fix and the wrong one — it would
    /// read an 8-hex serial *as* a dev_eui, which no JoinRequest can ever match, and the Hub
    /// would sit there looking healthy and answering nothing.
    #[test]
    fn a_serial_keyed_sessions_db_is_refused_with_a_wipe_hint() {
        let tmp = std::env::temp_dir().join(format!("p2pdb-predeveui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = Store::new(&tmp);
        let legacy = r#"{
          "nodes": {},
          "sessions": {
            "80e007b2": {
              "serial": "80e007b2",
              "dev_addr": 1,
              "session_key": "00112233445566778899aabbccddeeff",
              "last_join_at": null,
              "dev_nonce": 7,
              "uplink_counter": null,
              "state": "joined",
              "proto_version": 0,
              "downlink_queue": [],
              "downlink_phase": "idle"
            }
          },
          "cache": {}
        }"#;
        store.write(Slot::P2pSessions, legacy).expect("seed");

        let err = SessionsDb::load(&store).expect_err("a serial-keyed database is refused");
        assert_eq!(err.kind(), Kind::StorageError);
        assert_eq!(err.component(), Component::SessionDb);
        let m = err.message();
        assert!(m.contains("dev_eui"), "names the missing field: {m}");
        assert!(
            m.contains(&store.path(Slot::P2pSessions).display().to_string()),
            "names the file to delete: {m}"
        );
        assert!(
            m.contains("node-add"),
            "says how to get back to a working registry: {m}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The gap a sessions-only check leaves open. `RegisteredNode` is byte-for-byte unchanged
    /// by #73 — it always carried both `serial` and `dev_eui` — so a pre-#73 registry with no
    /// live sessions deserializes cleanly and the "missing dev_eui" signal never fires. What
    /// gives it away is the *key*: before #73 `nodes` was keyed by serial, after it by dev_eui.
    ///
    /// Left unrefused it is worse than a load failure: `--dev-eui` lookups miss a node that is
    /// registered, and node-add's duplicate guard misses too, so the same physical device can
    /// be registered twice and `on_join` then resolves to whichever row sorts first.
    #[test]
    fn a_serial_keyed_registry_with_no_sessions_is_refused() {
        let tmp = std::env::temp_dir().join(format!("p2pdb-nodesonly-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = Store::new(&tmp);
        // A bench Hub where node-add ran but nothing has joined yet.
        let legacy = r#"{
          "nodes": {
            "80e007b2": {
              "serial": "80e007b2",
              "dev_eui": "70b3d57ed0000abe",
              "profile": "default",
              "app_key": "00112233445566778899aabbccddeeff"
            }
          },
          "sessions": {},
          "cache": {}
        }"#;
        store.write(Slot::P2pSessions, legacy).expect("seed");

        let err = SessionsDb::load(&store).expect_err("a serial-keyed registry is refused");
        assert_eq!(err.kind(), Kind::StorageError);
        assert_eq!(err.component(), Component::SessionDb);
        assert!(
            err.message().contains("node-add"),
            "same wipe hint as the sessions case: {}",
            err.message()
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The other side of that invariant: a correctly-keyed document still loads.
    #[test]
    fn a_dev_eui_keyed_document_loads() {
        let tmp = std::env::temp_dir().join(format!("p2pdb-wellkeyed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = Store::new(&tmp);
        let mut db = SessionsDb::default();
        db.upsert_node(node("70b3d57ed0000abe", &"cc".repeat(16)));
        db.sessions
            .insert("70b3d57ed0000abe".into(), session("70b3d57ed0000abe", 1));
        db.save(&store).expect("save");

        let back = SessionsDb::load(&store).expect("a well-keyed document loads");
        assert_eq!(back, db);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The new form round-trips with its sequence intact — the other half of the migration.
    #[test]
    fn a_queued_downlink_round_trips_with_its_seq() {
        let mut db = SessionsDb::default();
        db.sessions
            .insert("70b3d57ed0000001".into(), session("70b3d57ed0000001", 1));
        db.enqueue_downlink(
            "70b3d57ed0000001",
            QueuedDownlink::with_seq(7, vec![0x08, 0x07]),
        );
        db.enqueue_downlink("70b3d57ed0000001", QueuedDownlink::raw(vec![0xaa]));
        let json = serde_json::to_string(&db).expect("serialize");
        let back: SessionsDb = serde_json::from_str(&json).expect("deserialize");
        let q = &back.sessions["70b3d57ed0000001"].downlink_queue;
        assert_eq!(q[0].seq, Some(7));
        assert_eq!(q[1].seq, None);
        assert_eq!(q[1].body, vec![0xaa]);
    }

    /// F2: the lost-update race. Two writers doing load-mutate-save without a lock keep
    /// whichever saved last and silently drop the other's work — the reason a `node-send`
    /// issued while `control-radio-p2p-host` was running never reached the node on
    /// 2026-08-28. Under [`Store::lock`] every one of the 200 registrations survives.
    #[test]
    fn concurrent_updates_are_not_lost() {
        let tmp = std::env::temp_dir().join(format!("p2pdb-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = Store::new(&tmp);

        let writers: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|tag| {
                let store = store.clone();
                std::thread::spawn(move || {
                    for i in 0..100 {
                        let _db_lock = store.lock(Slot::P2pSessions).expect("lock");
                        let mut db = SessionsDb::load(&store).expect("load");
                        db.upsert_node(node(&format!("{tag}{i:015x}"), &"aa".repeat(16)));
                        db.save(&store).expect("save");
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().expect("writer finished");
        }

        let db = SessionsDb::load(&store).expect("final load");
        assert_eq!(db.nodes.len(), 200, "no registration was overwritten");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The asymmetric half of the same race, in the shape the bench hit it: a long
    /// runtime-style critical section (load, think, save) with a CLI `node-send` arriving in
    /// the middle. The CLI write must be visible in what the runtime saves, not clobbered by
    /// the snapshot the runtime read before it.
    #[test]
    fn a_cli_write_inside_a_runtime_critical_section_is_not_clobbered() {
        let tmp = std::env::temp_dir().join(format!("p2pdb-interleave-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = Store::new(&tmp);
        {
            let mut db = SessionsDb::default();
            db.upsert_node(node("70b3d57e80e007b2", &"aa".repeat(16)));
            db.sessions
                .insert("70b3d57e80e007b2".into(), session("70b3d57e80e007b2", 1));
            db.save(&store).expect("seed");
        }

        // The CLI: enqueue a downlink, exactly as `node-send` does.
        let cli = {
            let store = store.clone();
            std::thread::spawn(move || {
                let _db_lock = store.lock(Slot::P2pSessions).expect("lock");
                let mut db = SessionsDb::load(&store).expect("load");
                db.enqueue_downlink("70b3d57e80e007b2", QueuedDownlink::raw(vec![0x11, 0x22]))
                    .expect("live session");
                db.save(&store).expect("save");
            })
        };

        // The runtime: advance the counter over a window wide enough for the CLI to land in.
        {
            let _db_lock = store.lock(Slot::P2pSessions).expect("lock");
            let mut db = SessionsDb::load(&store).expect("load");
            std::thread::sleep(std::time::Duration::from_millis(50));
            if let Some(s) = db.session_by_addr_mut(1) {
                s.last_seen = 7;
            }
            db.save(&store).expect("save");
        }
        cli.join().expect("cli finished");

        let db = SessionsDb::load(&store).expect("final load");
        let s = db.sessions.get("70b3d57e80e007b2").expect("session");
        assert_eq!(
            s.downlink_queue.front().map(|v| v.body.as_slice()),
            Some([0x11, 0x22].as_slice()),
            "the CLI's queued downlink survived the runtime's save"
        );
        assert_eq!(s.last_seen, 7, "and so did the counter");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn upsert_rotates_the_key_on_re_add() {
        let mut db = SessionsDb::default();
        db.upsert_node(node("70b3d57ed0000001", "aa".repeat(16).as_str()));
        db.upsert_node(node("70b3d57ed0000001", "bb".repeat(16).as_str()));
        assert_eq!(db.nodes.len(), 1, "re-add is an upsert, not a second row");
        assert_eq!(
            db.node("70b3d57ed0000001").unwrap().app_key,
            "bb".repeat(16)
        );
        // And the serial, now an attribute, still finds the same record.
        assert_eq!(
            db.node_by_serial("80e007b2").unwrap().app_key,
            "bb".repeat(16)
        );
    }

    #[test]
    fn round_trips_through_the_store() {
        let tmp = std::env::temp_dir().join(format!("p2pdb-rt-{}", std::process::id()));
        let store = Store::new(&tmp);
        let mut db = SessionsDb::default();
        db.upsert_node(node("70b3d57ed0000001", &"cc".repeat(16)));
        db.sessions
            .insert("70b3d57ed0000001".into(), session("70b3d57ed0000001", 1));
        db.save(&store).expect("save");

        let back = SessionsDb::load(&store).expect("load");
        assert_eq!(back, db);
        assert_eq!(back.session_by_addr(1).unwrap().dev_eui, "70b3d57ed0000001");
        assert!(back.session_by_addr(2).is_none());

        // Secret mode on the on-disk file.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path(Slot::P2pSessions))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "sessions.db holds key material");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    #[allow(deprecated)]
    fn remove_node_returns_its_session_and_leaves_it_to_the_caller() {
        let mut db = SessionsDb::default();
        db.upsert_node(node("70b3d57ed0000001", &"dd".repeat(16)));
        db.sessions
            .insert("70b3d57ed0000001".into(), session("70b3d57ed0000001", 5));
        db.cache
            .insert("70b3d57ed0000001".into(), NodeCache::default());

        let (removed, session) = db.remove_node("70b3d57ed0000001");
        assert!(removed.is_some());
        assert_eq!(session.expect("had a session").addr, 5);
        assert!(db.nodes.is_empty(), "the registry entry is gone");
        assert!(
            !db.cache.contains_key("70b3d57ed0000001"),
            "cache is dropped with the node"
        );
        // Left in place: `node-remove` drops it itself; a caller may still need it.
        assert!(db.sessions.contains_key("70b3d57ed0000001"));

        // Removing the (already-gone) registry entry again is not an error — the node is None.
        let (gone, _still_pending) = db.remove_node("70b3d57ed0000001");
        assert!(
            gone.is_none(),
            "removing the registry entry twice is not an error"
        );
    }

    /// Q-N4: parking keeps the session the Node still holds and drops only the state of the
    /// P2P link's downlinks.
    #[test]
    fn parking_keeps_the_session_and_drops_its_link_state() {
        let mut db = SessionsDb::default();
        let mut s = session("5876070000005722", 7);
        s.last_seen = 41;
        s.downlink_queue = [QueuedDownlink::with_seq(3, vec![0x03])]
            .into_iter()
            .collect();
        s.downlink_queue[0].uplinks_since_sent = 2;
        s.downlink_phase = DownlinkPhase::Sent;
        s.history = Some(crate::p2p::session::HistoryCollect::new(3, "t", 1));
        db.sessions.insert(s.dev_eui.clone(), s.clone());

        assert!(db.park_session("5876070000005722"));
        let parked = &db.sessions["5876070000005722"];
        assert!(parked.downlink_queue.is_empty());
        assert_eq!(parked.downlink_phase, DownlinkPhase::Idle);
        assert!(parked.history.is_none());
        assert_eq!(
            (
                parked.addr,
                &parked.session_key,
                parked.last_seen,
                parked.dev_nonce
            ),
            (7, &s.session_key, s.last_seen, s.dev_nonce),
            "the session itself is untouched"
        );
        assert!(!db.park_session("0000000000000000"), "no session to park");
    }

    /// Parked means held here while the Node is off the P2P view.
    #[test]
    fn a_session_is_parked_only_while_its_node_is_off_the_p2p_view() {
        let id = "5876070000005722";
        let mut db = SessionsDb::default();
        assert!(!db.is_parked(id), "no session");
        db.sessions.insert(id.into(), session(id, 7));
        assert!(db.is_parked(id), "not on the P2P view");
        db.upsert_node(node(id, &"aa".repeat(16)));
        assert!(!db.is_parked(id), "on the P2P view: live");
    }

    #[test]
    fn enqueue_downlink_appends_to_a_live_session_and_needs_one() {
        let mut db = SessionsDb::default();
        db.upsert_node(node("70b3d57ed0000001", &"ee".repeat(16)));
        // No session yet: a downlink cannot be sealed against a node that has not joined.
        assert_eq!(
            db.enqueue_downlink("70b3d57ed0000001", QueuedDownlink::raw(vec![0x01])),
            None
        );

        db.sessions
            .insert("70b3d57ed0000001".into(), session("70b3d57ed0000001", 9));
        assert_eq!(
            db.enqueue_downlink("70b3d57ed0000001", QueuedDownlink::raw(vec![0x01, 0x02])),
            Some(1)
        );
        assert_eq!(
            db.enqueue_downlink("70b3d57ed0000001", QueuedDownlink::raw(vec![0x03])),
            Some(2),
            "FIFO grows"
        );
        let q = &db.sessions.get("70b3d57ed0000001").unwrap().downlink_queue;
        assert_eq!(
            q.front().map(|v| v.body.as_slice()),
            Some([0x01, 0x02].as_slice())
        );
        assert_eq!(q.back().map(|v| v.body.as_slice()), Some([0x03].as_slice()));
    }
}
