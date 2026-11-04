//! The `node-get` cache — `plan/control/radio/plan.md` §5 and the "Runtime form" section.
//!
//! > per node, the last decoded reading, device info, alarm batch and config pages as JSON
//! > files under `/data/proximos/radio/{lorawan|p2p}/cache/{id}.json` (declared paths — rows in
//! > the §Sandboxing union table; survives process restarts, wiped by `clear`/`wipe`).
//!
//! One file per node, not one per slot: `node-get` addresses a node and asks for one of its
//! five views, so the unit an operator (and `wipe`) deletes is the node. Five files would mean
//! five chances to leave one behind.
//!
//! **Not a [`crate::store::Slot`].** `Slot` is deliberately a closed enum so the Landlock and
//! AppArmor tables can be reviewed against the code; the cache is keyed by device identifier,
//! which is unbounded and cannot be enumerated. What is enumerable is the *directory*, so that
//! is what this module owns and what the path tables name — and [`Cache::sanitize`] is what
//! keeps an identifier from turning the directory row into a whole-filesystem row.
//!
//! Writes go through the same temp-and-rename discipline [`crate::store`] uses, for the same
//! reason: a torn cache file after a power cut would read as a `StorageError` on the next
//! `node-get` rather than as the stale-but-valid reading it actually is.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::decode::command::{CommandTag, SeqAllocator};
use crate::error::{Component, Error, Kind, Result, Transport};

/// The directory the cache lives in, under the store root. One for both carriers since the
/// one Node registry (node_registry.md §3, R5): a Node is on one carrier, and what it reported
/// — its config, its device info, the command sequence — survives a move.
pub const CACHE_DIR: &str = "cache";

/// Marks `cache/` as holding the merged per-carrier caches, so the merge runs once.
const MIGRATED_MARKER: &str = ".merged-from-carriers";

/// Which of `node-get`'s five views a document holds — `plan.md` §5's `<type>` table.
///
/// The spelling is the JSON key *and* the CLI value, so the two cannot drift: `device-info` on
/// the command line, `device_info` in the file, one enum in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheSlot {
    /// The last received uplink/telemetry message (fPort 2).
    Message,
    /// Device info received over radio (the fPort-85 `Info` response).
    DeviceInfo,
    /// The last received alarm batch (fPort 3).
    Alarm,
    /// Device config, settable keys (a reassembled fPort-85 `ConfigDump`).
    Config,
    /// Full config dump.
    FullConfig,
}

impl CacheSlot {
    pub const fn as_str(self) -> &'static str {
        match self {
            CacheSlot::Message => "message",
            CacheSlot::DeviceInfo => "device_info",
            CacheSlot::Alarm => "alarm",
            CacheSlot::Config => "config",
            CacheSlot::FullConfig => "full_config",
        }
    }

    pub const ALL: [CacheSlot; 5] = [
        CacheSlot::Message,
        CacheSlot::DeviceInfo,
        CacheSlot::Alarm,
        CacheSlot::Config,
        CacheSlot::FullConfig,
    ];
}

impl From<crate::cli::NodeGetType> for CacheSlot {
    fn from(t: crate::cli::NodeGetType) -> Self {
        use crate::cli::NodeGetType;
        match t {
            NodeGetType::Message => CacheSlot::Message,
            NodeGetType::DeviceInfo => CacheSlot::DeviceInfo,
            NodeGetType::Alarm => CacheSlot::Alarm,
            NodeGetType::Config => CacheSlot::Config,
            NodeGetType::FullConfig => CacheSlot::FullConfig,
        }
    }
}

/// One slot's contents plus when it was written.
///
/// The timestamp is not decoration: `node-get` answers with radio-reported data that may be
/// hours old, and a consumer that cannot see *how* old would present a stale config dump as
/// the device's current state.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CacheEntry {
    /// RFC3339, when this slot was last written.
    pub at: String,
    pub value: Value,
}

/// One node's cache document.
///
/// Unknown keys are **kept**, not rejected: an older `proximos-control` reading a file a newer
/// one wrote must not drop the slots it does not know about, or a downgrade would silently
/// erase them on the next merge.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CacheDocument {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(flatten)]
    pub slots: Map<String, Value>,
}

impl CacheDocument {
    /// Read one slot, or `None` when nothing has filled it yet.
    pub fn slot(&self, slot: CacheSlot) -> Option<CacheEntry> {
        self.slots
            .get(slot.as_str())
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// Which slots have been filled, in [`CacheSlot::ALL`] order.
    pub fn filled(&self) -> Vec<CacheSlot> {
        CacheSlot::ALL
            .into_iter()
            .filter(|s| self.slots.contains_key(s.as_str()))
            .collect()
    }

    /// When this carrier last heard the node: the newest `at` across its filled slots.
    ///
    /// Any slot is proof of a frame the node sent, so this is the retained `info`'s
    /// `last_seen` on both arms, and what a `dual` Hub compares to pick the carrier a node on
    /// both is live on ([`crate::mode::live_carrier`], p2p_integration.md §11). The Hub's own
    /// bookkeeping ([`COMMAND_SEQ_KEY`], [`SENT_COMMANDS_KEY`]) is not a slot, so a command
    /// queued for the node does not count as hearing it.
    pub fn last_heard(&self) -> Option<String> {
        self.filled()
            .into_iter()
            .filter_map(|s| self.slot(s))
            .map(|e| e.at)
            .max_by_key(|at| at_order_key(at))
    }

    /// What ChirpStack's last `event/join` for this Node said, if the Node has joined over
    /// LoRaWAN since its last move ([`LORAWAN_SESSION_KEY`]).
    pub fn lorawan_session(&self) -> Option<LorawanSession> {
        self.slots
            .get(LORAWAN_SESSION_KEY)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// The command tag this Hub last sent under `seq`, if it still remembers it — see
    /// [`SENT_COMMANDS_KEY`]. `None` for `seq` 0 (the Node's own unsolicited frames), for a raw
    /// `--hex` downlink (no sequence allocated) and for one older than the last
    /// [`SENT_COMMANDS_KEPT`].
    pub fn sent_tag(&self, seq: u32) -> Option<u32> {
        self.sent_entry(seq)
            .and_then(|e| e.get("tag"))
            .and_then(Value::as_u64)
            .map(|t| t as u32)
    }

    /// Whether this Hub sent the frame under `seq` as a continuation of an earlier exchange,
    /// under the operator's `--seq` rather than a fresh one ([`Cache::record_sent`]). Such a
    /// frame completes that exchange's answer, so a newer ask never coalesces it
    /// ([`crate::coalesce::kind`]).
    pub fn is_continuation(&self, seq: u32) -> bool {
        self.sent_entry(seq)
            .and_then(|e| e.get(CONTINUATION_KEY))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Whether the frame under `seq` belongs to a `set-config --supersede`: a Node's whole
    /// pending configuration, which a newer one withdraws while it waits
    /// ([`crate::coalesce::replaces_config`], config_sync.md §13 Q20b).
    pub fn is_superseding(&self, seq: u32) -> bool {
        self.sent_entry(seq)
            .and_then(|e| e.get(SUPERSEDE_KEY))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// The largest downlink the network server last said it would carry to this Node, while
    /// that word is fresh ([`PAYLOAD_CAP_KEY`], [`Cache::note_payload_cap`], #166): `node-send`
    /// packs a `set-config` into no more than this, whatever the last uplink's data rate says.
    pub fn payload_cap(&self, now_unix: i64) -> Option<usize> {
        let cap = self.slots.get(PAYLOAD_CAP_KEY)?;
        let at = cap.get("at").and_then(Value::as_i64)?;
        if now_unix.saturating_sub(at) > PAYLOAD_CAP_TTL_S {
            return None;
        }
        cap.get("max").and_then(Value::as_u64).map(|m| m as usize)
    }

    /// The `seq` a LoRaWAN frame was sent under, from the ChirpStack queue-item id its enqueue
    /// returned ([`DOWNLINK_ID_KEY`], [`Cache::note_downlink_id`]). `None` for a raw `--hex`
    /// frame (no sequence) and for one older than the last [`SENT_COMMANDS_KEPT`].
    pub fn seq_for_downlink(&self, downlink_id: &str) -> Option<u32> {
        if downlink_id.is_empty() {
            return None;
        }
        self.slots
            .get(SENT_COMMANDS_KEY)?
            .as_array()?
            .iter()
            .rev()
            .find(|e| e.get(DOWNLINK_ID_KEY).and_then(Value::as_str) == Some(downlink_id))
            .and_then(|e| e.get("seq"))
            .and_then(Value::as_u64)
            .map(|s| s as u32)
    }

    /// The newest [`SENT_COMMANDS_KEY`] entry for `seq`: a wrapped sequence used again later is
    /// the later command.
    fn sent_entry(&self, seq: u32) -> Option<&Value> {
        if seq == 0 {
            return None;
        }
        self.slots
            .get(SENT_COMMANDS_KEY)?
            .as_array()?
            .iter()
            .rev()
            .find(|e| e.get("seq").and_then(Value::as_u64) == Some(u64::from(seq)))
    }
}

/// A cache `at` as a key that sorts in time order.
///
/// The timestamps are all this crate's own UTC `…Z` spelling, but with and without a fraction
/// (ChirpStack's event time carries microseconds, a P2P arrival stamp whole seconds), and a
/// plain string compare would put `…:05Z` after `…:05.100000Z`. A whole-second stamp is padded
/// to microseconds.
pub fn at_order_key(at: &str) -> String {
    match at.strip_suffix('Z') {
        Some(head) if !head.contains('.') => format!("{head}.000000Z"),
        _ => at.to_string(),
    }
}

/// The cache directory, one for both carriers.
#[derive(Debug, Clone)]
pub struct Cache {
    dir: PathBuf,
    /// The carrier the caller acts for — what an error names. It no longer picks the
    /// directory.
    transport: Transport,
}

impl Cache {
    /// `{store_root}/cache/`. The first call merges the per-carrier caches a pre-registry
    /// build kept under `{lorawan|p2p}/cache/` ([`merge_legacy`]).
    pub fn new(store: &crate::store::Store, transport: Transport) -> Self {
        let dir = store.root().join(CACHE_DIR);
        // Only under a store root that exists: a path computed for a report must not create
        // the tree.
        if store.root().exists() && !dir.join(MIGRATED_MARKER).exists() {
            if let Err(e) = merge_legacy(store.root(), &dir) {
                eprintln!("[cache] merging the per-carrier caches failed: {e}");
            }
        }
        Cache { dir, transport }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The per-carrier directory a pre-registry build kept, `{store_root}/{lorawan|p2p}/cache/`,
    /// without the merge [`Cache::new`] runs. Only the registry migration reads it, for what each
    /// carrier *separately* last heard; the merged document cannot tell them apart.
    pub fn legacy(store: &crate::store::Store, transport: Transport) -> Self {
        Cache {
            dir: store.root().join(transport.as_str()).join(CACHE_DIR),
            transport,
        }
    }

    /// The file one node's document lives in.
    pub fn path(&self, id: &str) -> Result<PathBuf> {
        Ok(self.dir.join(format!("{}.json", self.sanitize(id)?)))
    }

    /// Reject an identifier that could name a file outside the cache directory.
    ///
    /// Both carriers key their documents by `dev_eui` since GitLab #73, and a `dev_eui` is
    /// 16 hex characters — so in practice nothing reaching here is hostile any more. The check
    /// stays because it is the only place a bad identifier can be stopped once:
    /// `../../lorawan/chirpstack-api-token-admin` as an id would turn a cache write into a
    /// token overwrite, and the cost of keeping the guard is one comparison.
    fn sanitize<'a>(&self, id: &'a str) -> Result<&'a str> {
        let ok = !id.is_empty()
            && id.len() <= 64
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if ok {
            Ok(id)
        } else {
            Err(Error::new(
                Kind::InvalidArgument,
                self.transport,
                Component::EnrollmentRecord,
                format!(
                    "{id:?} is not usable as a cache key: only ASCII letters, digits, '-' and \
                     '_' are, up to 64 characters — anything else could name a file outside \
                     the cache directory"
                ),
            ))
        }
    }

    fn err(&self, path: &Path, why: String) -> Error {
        Error::new(
            Kind::StorageError,
            self.transport,
            Component::EnrollmentRecord,
            format!("{}: {why}", path.display()),
        )
    }

    /// Read one node's document. `Ok(None)` when the node has never been heard from.
    ///
    /// Absent and unreadable stay different, exactly as in [`crate::store::Store::read`]:
    /// absent is "no uplink yet", unreadable is a `StorageError` an operator has to act on.
    pub fn read(&self, id: &str) -> Result<Option<CacheDocument>> {
        let path = self.path(id)?;
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(self.err(&path, format!("cannot read: {e}"))),
        };
        if raw.trim().is_empty() {
            return Ok(None);
        }
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|e| self.err(&path, format!("the cache document is not readable: {e}")))
    }

    /// Write one slot, leaving every other slot alone.
    ///
    /// Merge rather than replace, because the five slots are filled by different frames
    /// arriving at different times: an fPort-2 telemetry uplink must not erase the config dump
    /// a `node-send get-config` spent minutes collecting.
    ///
    /// A page of a paged telemetry report folds into the `message` it continues (#126, see
    /// [`crate::decode::reading::fold_message_page`]), so `node-get message` shows the whole
    /// report rather than its last page.
    pub fn merge_write(&self, id: &str, slot: CacheSlot, at: &str, value: Value) -> Result<()> {
        let mut doc = self.read(id)?.unwrap_or_default();
        doc.id = id.to_string();
        let value = if slot == CacheSlot::Message {
            let held = doc.slot(CacheSlot::Message).map(|e| e.value);
            crate::decode::reading::fold_message_page(held.as_ref(), value)
        } else {
            value
        };
        let entry = CacheEntry {
            at: at.to_string(),
            value,
        };
        doc.slots.insert(
            slot.as_str().to_string(),
            serde_json::to_value(&entry).map_err(|e| {
                Error::new(
                    Kind::StorageError,
                    self.transport,
                    Component::EnrollmentRecord,
                    format!("the cache entry could not be serialized: {e}"),
                )
            })?,
        );
        self.write_document(id, &doc)
    }

    fn write_document(&self, id: &str, doc: &CacheDocument) -> Result<()> {
        let path = self.path(id)?;
        let json = serde_json::to_string_pretty(doc).map_err(|e| {
            Error::new(
                Kind::StorageError,
                self.transport,
                Component::EnrollmentRecord,
                format!("the cache document could not be serialized: {e}"),
            )
        })?;

        std::fs::create_dir_all(&self.dir)
            .map_err(|e| self.err(&self.dir, format!("cannot create the cache directory: {e}")))?;

        // Same temp-and-rename as `store::Store::write`, for the same reason: a torn document
        // would read as a StorageError on the next `node-get` rather than as the stale-but-
        // valid reading it is. The directory fsync `store` does is deliberately *not* repeated
        // — the cache is a convenience that a restart refills from the next uplink, and paying
        // a directory sync per uplink would be a real cost for a file nothing depends on.
        let tmp = tmp_path(&path, std::process::id());
        {
            let mut f = std::fs::File::create(&tmp)
                .map_err(|e| self.err(&tmp, format!("cannot create a temp file: {e}")))?;
            f.write_all(json.as_bytes())
                .map_err(|e| self.err(&tmp, format!("cannot write: {e}")))?;
            f.sync_all()
                .map_err(|e| self.err(&tmp, format!("cannot flush: {e}")))?;
        }
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            self.err(&path, format!("cannot replace: {e}"))
        })
    }

    /// Drop one node's cache. Absence is success — `node-remove` must be re-runnable.
    pub fn remove(&self, id: &str) -> Result<()> {
        let path = self.path(id)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(self.err(&path, format!("cannot remove: {e}"))),
        }
    }

    /// Drop the caches of `ids` — what `clear`/`wipe` of one carrier owe, now that both
    /// carriers share the directory.
    pub fn remove_all(&self, ids: &[String]) -> Result<()> {
        for id in ids {
            self.remove(id)?;
        }
        Ok(())
    }

    /// Drop every node's cache, both carriers'.
    ///
    /// The directory itself survives: the migration created it, and removing it would make the
    /// next uplink depend on `create_dir_all` succeeding under whatever confinement is active
    /// at the time.
    pub fn wipe(&self) -> Result<()> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(self.err(&self.dir, format!("cannot list: {e}"))),
        };
        for entry in entries {
            let entry = entry.map_err(|e| self.err(&self.dir, format!("cannot list: {e}")))?;
            let path = entry.path();
            // Only this module's own files. A stray directory is left alone rather than
            // recursively removed — `wipe` deletes enrolled devices, not whatever else has
            // found its way into `/data`.
            if path.extension().is_some_and(|x| x == "json" || x == "tmp") {
                std::fs::remove_file(&path)
                    .map_err(|e| self.err(&path, format!("cannot remove: {e}")))?;
            }
        }
        Ok(())
    }

    /// Every node this cache holds a document for, sorted.
    pub fn ids(&self) -> Result<Vec<String>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(self.err(&self.dir, format!("cannot list: {e}"))),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|x| x == "json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    out.push(stem.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Allocate this node's next fPort-85 command sequence, persistently.
    ///
    /// §9.7 requires "a per-device, monotonically increasing sequence number". A
    /// `proximosctl node-send` is a process that exits, so an in-memory counter would restart
    /// at 1 on every invocation and two commands minutes apart would carry the same sequence —
    /// which is the one thing correlation cannot survive. The high-water mark therefore lives
    /// in the node's own cache document, next to the responses it correlates with, under the
    /// same declared directory row (no new path to add to the Landlock/AppArmor tables) and
    /// wiped by the same `clear`/`wipe` that drops the device.
    ///
    /// The allocation rule itself is *not* re-implemented here — [`SeqAllocator`] owns it, so
    /// "never issue 0" is stated once.
    ///
    /// `tag` is recorded under the sequence in the same write ([`SENT_COMMANDS_KEY`]), so the
    /// uplink path can tell what a response answers.
    pub fn allocate_seq(&self, id: &str, tag: CommandTag) -> Result<u32> {
        let mut doc = self.read(id)?.unwrap_or_default();
        doc.id = id.to_string();
        let last = doc
            .slots
            .get(COMMAND_SEQ_KEY)
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;

        let mut allocator = SeqAllocator::from_last(last);
        let seq = allocator.allocate();

        doc.slots
            .insert(COMMAND_SEQ_KEY.to_string(), Value::from(seq));
        push_sent(&mut doc, seq, tag);

        self.write_document(id, &doc)?;
        Ok(seq)
    }

    /// Record a frame sent under a sequence this call did not allocate — a re-request under
    /// an exchange's original `seq`. Every sent frame is recorded, whichever path chose its
    /// sequence, so [`CacheDocument::sent_tag`] never silently falls back to "merge" for a
    /// `get-config` the Hub did send.
    ///
    /// The entry is marked a continuation ([`CacheDocument::is_continuation`]): the frame
    /// completes the answer of the exchange it re-requests, which a newer ask must not
    /// coalesce away.
    pub fn record_sent(&self, id: &str, seq: u32, tag: CommandTag) -> Result<()> {
        let mut doc = self.read(id)?.unwrap_or_default();
        doc.id = id.to_string();
        push_sent(&mut doc, seq, tag);
        if let Some(entry) = sent_entry_mut(&mut doc, seq) {
            entry[CONTINUATION_KEY] = Value::Bool(true);
        }
        self.write_document(id, &doc)
    }

    /// Keep the session a LoRaWAN join set up ([`LORAWAN_SESSION_KEY`]), replacing the one
    /// before it: a rejoin is a new session.
    pub fn set_lorawan_session(&self, id: &str, session: &LorawanSession) -> Result<()> {
        let mut doc = self.read(id)?.unwrap_or_default();
        doc.id = id.to_string();
        let value = serde_json::to_value(session).map_err(|e| {
            Error::new(
                Kind::StorageError,
                self.transport,
                Component::EnrollmentRecord,
                format!("the LoRaWAN session could not be serialized: {e}"),
            )
        })?;
        doc.slots.insert(LORAWAN_SESSION_KEY.to_string(), value);
        self.write_document(id, &doc)
    }

    /// Forget the LoRaWAN session — what `node-move` owes, since the session belonged to the
    /// carrier the Node left (node_registry.md §5, amended 2026-09-28). A Node with no document
    /// or no session is left as it is, and no file is created for it.
    pub fn clear_lorawan_session(&self, id: &str) -> Result<()> {
        let Some(mut doc) = self.read(id)? else {
            return Ok(());
        };
        if doc.slots.remove(LORAWAN_SESSION_KEY).is_none() {
            return Ok(());
        }
        self.write_document(id, &doc)
    }

    /// Record on `seq`'s [`SENT_COMMANDS_KEY`] entry that a newer frame, sent under `by`, took
    /// its place in the queue (`superseded_by`, plan.md §5 `node-send`, amended 2026-09-28). The
    /// Node answers `by`, never `seq`. A seq with no entry left is left alone.
    pub fn note_superseded(&self, id: &str, seq: u32, by: u32) -> Result<()> {
        let mut doc = self.read(id)?.unwrap_or_default();
        doc.id = id.to_string();
        let Some(entry) = sent_entry_mut(&mut doc, seq) else {
            return Ok(());
        };
        entry["superseded_by"] = Value::from(by);
        self.write_document(id, &doc)
    }
}

impl Cache {
    /// Record on `seq`'s [`SENT_COMMANDS_KEY`] entry the ChirpStack queue-item id its enqueue
    /// returned ([`DOWNLINK_ID_KEY`]), so a later `event/log` naming that item can be told
    /// which command it was (#129). A seq with no entry left is left alone.
    pub fn note_downlink_id(&self, id: &str, seq: u32, downlink_id: &str) -> Result<()> {
        let mut doc = self.read(id)?.unwrap_or_default();
        doc.id = id.to_string();
        let Some(entry) = sent_entry_mut(&mut doc, seq) else {
            return Ok(());
        };
        entry[DOWNLINK_ID_KEY] = Value::from(downlink_id);
        self.write_document(id, &doc)
    }
}

impl Cache {
    /// Remember that the network server discarded a downlink to this Node as larger than the
    /// `max` bytes its current data rate carries (#166). The next `set-config` is packed into
    /// `max` ([`CacheDocument::payload_cap`]), so the resend goes as smaller frames.
    pub fn note_payload_cap(&self, id: &str, max: u64, now_unix: i64) -> Result<()> {
        let mut doc = self.read(id)?.unwrap_or_default();
        doc.id = id.to_string();
        doc.slots.insert(
            PAYLOAD_CAP_KEY.to_string(),
            serde_json::json!({ "max": max, "at": now_unix }),
        );
        self.write_document(id, &doc)
    }
}

impl Cache {
    /// Marks the frame sent under `seq` as part of a `set-config --supersede`
    /// ([`CacheDocument::is_superseding`]). A seq this document no longer remembers is left alone.
    pub fn note_superseding(&self, id: &str, seq: u32) -> Result<()> {
        let mut doc = self.read(id)?.unwrap_or_default();
        doc.id = id.to_string();
        let Some(entry) = sent_entry_mut(&mut doc, seq) else {
            return Ok(());
        };
        entry[SUPERSEDE_KEY] = Value::Bool(true);
        self.write_document(id, &doc)
    }
}

/// [`CacheDocument::sent_entry`], for writing.
fn sent_entry_mut(doc: &mut CacheDocument, seq: u32) -> Option<&mut Value> {
    doc.slots
        .get_mut(SENT_COMMANDS_KEY)?
        .as_array_mut()?
        .iter_mut()
        .rev()
        .find(|e| e.get("seq").and_then(Value::as_u64) == Some(u64::from(seq)))
}

/// Append `{seq, tag}` to a document's [`SENT_COMMANDS_KEY`], keeping the latest
/// [`SENT_COMMANDS_KEPT`]. Oldest first; a wrapped sequence used again later is found
/// newest-first by [`CacheDocument::sent_tag`].
fn push_sent(doc: &mut CacheDocument, seq: u32, tag: CommandTag) {
    let mut sent: Vec<Value> = doc
        .slots
        .get(SENT_COMMANDS_KEY)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    sent.push(serde_json::json!({ "seq": seq, "tag": tag as u32 }));
    if sent.len() > SENT_COMMANDS_KEPT {
        sent.drain(..sent.len() - SENT_COMMANDS_KEPT);
    }
    doc.slots
        .insert(SENT_COMMANDS_KEY.to_string(), Value::Array(sent));
}

/// Where [`Cache::allocate_seq`] keeps the per-device sequence high-water mark.
///
/// Deliberately not a [`CacheSlot`]: the five slots are *what the device reported*, which is
/// what `node-get` reads and `filled()` enumerates. This is Hub-side bookkeeping that happens
/// to be keyed by the same node, and `CacheDocument` keeps unknown keys precisely so a
/// document can carry both without either side losing the other's.
pub const COMMAND_SEQ_KEY: &str = "command_seq";

/// Where [`Cache::allocate_seq`] records which command each recent sequence was sent as:
/// `[{"seq": 117, "tag": 5}, …]`, oldest first.
///
/// A response names only the sequence it answers, and two different questions can come back
/// in the same shape: a `get-config` and the Node's own settings-info are both a `ConfigDump`.
/// They must not be treated alike. A complete `get-config` answer is the Node's whole
/// configuration, and a key it leaves out is gone (an emptied alarm slot sends no field at
/// all). Every other dump is a subset. The uplink path reads this to tell them apart
/// (`node_reported_state.md` §2.2). Hub-side bookkeeping like [`COMMAND_SEQ_KEY`], not a slot.
pub const SENT_COMMANDS_KEY: &str = "sent_commands";

/// Where the LoRaWAN arm keeps the session ChirpStack's last `event/join` set up:
/// `{"dev_addr": "01ab23cd", "joined_at": "…Z"}` (node_registry.md §5, amended 2026-09-28,
/// Q-N2).
///
/// Not a [`CacheSlot`], so not something `node-get` reads or `last_heard` counts: the slots are
/// what the Node reported in its own frames, and this is what the network server made of a
/// JoinRequest. It sits in the Node's document rather than in a store slot of its own because
/// it has to be dropped with the Node (`node-remove`, `clear`, `wipe`) and because the
/// connect-time replay already reads this document for the rest of the Node's `info`.
pub const LORAWAN_SESSION_KEY: &str = "lorawan_session";

/// The two session facts the LoRaWAN `info` carries, as [`LORAWAN_SESSION_KEY`] holds them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LorawanSession {
    /// ChirpStack's spelling: 8 lowercase hex digits. The P2P `dev_addr` is 8 uppercase hex digits.
    pub dev_addr: String,
    /// RFC 3339 UTC, the join event's `time`.
    pub joined_at: String,
}

/// How many sent commands [`SENT_COMMANDS_KEY`] keeps per node. A handful of exchanges can be
/// in the air at once on a Class-A link; an answer to anything older is merged, which is the
/// safe side of the rule.
pub const SENT_COMMANDS_KEPT: usize = 16;

/// The [`SENT_COMMANDS_KEY`] entry field naming the ChirpStack queue item a LoRaWAN frame was
/// enqueued as — the `downlink_id` `node-send` answers with, and the `queue_item_id` of the
/// `event/log` ChirpStack raises if it discards that item (#129).
pub const DOWNLINK_ID_KEY: &str = "downlink_id";

/// The Node document's note of the largest downlink the network server would carry
/// (`{"max": bytes, "at": unix}`, #166). Hub-side bookkeeping, not a slot.
pub const PAYLOAD_CAP_KEY: &str = "payload_cap";

/// How long a [`PAYLOAD_CAP_KEY`] note holds. ADR may raise the data rate again; a day later
/// the last uplink's data rate is trusted again, and a refusal sets a new cap.
pub const PAYLOAD_CAP_TTL_S: i64 = 24 * 3600;

/// The [`SENT_COMMANDS_KEY`] entry flag [`Cache::record_sent`] sets: the frame went out under
/// an earlier exchange's `seq` to complete it.
pub const CONTINUATION_KEY: &str = "continuation";

/// The [`SENT_COMMANDS_KEY`] entry flag of a `set-config --supersede` frame.
pub const SUPERSEDE_KEY: &str = "supersede";
/// The temp file [`Cache::write_document`] renames from, qualified by the writing process.
///
/// Two processes write the same node's document — the runtime on an uplink, the CLI on a
/// `node-send` — and a shared `{id}.tmp` lets one `File::create` truncate the other's
/// half-written file and then rename the torn result over the real one. The pid is a
/// parameter rather than read inside so the property is testable.
fn tmp_path(path: &Path, pid: u32) -> PathBuf {
    path.with_extension(format!("{pid}.tmp"))
}

/// Merge two documents of one Node: each reported slot keeps the newer entry, the command
/// sequence the higher high-water mark, and the sent-command log both carriers' entries (the
/// latest [`SENT_COMMANDS_KEPT`]). Any other key keeps `into`'s value when it has one.
pub fn merge_documents(into: &mut CacheDocument, from: CacheDocument) {
    for (key, value) in from.slots {
        match key.as_str() {
            COMMAND_SEQ_KEY => {
                let a = into.slots.get(&key).and_then(Value::as_u64).unwrap_or(0);
                let b = value.as_u64().unwrap_or(0);
                into.slots.insert(key, Value::from(a.max(b)));
            }
            SENT_COMMANDS_KEY => {
                let mut sent: Vec<Value> = into
                    .slots
                    .get(&key)
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                sent.extend(value.as_array().cloned().unwrap_or_default());
                if sent.len() > SENT_COMMANDS_KEPT {
                    sent.drain(..sent.len() - SENT_COMMANDS_KEPT);
                }
                into.slots.insert(key, Value::Array(sent));
            }
            k if CacheSlot::ALL.iter().any(|s| s.as_str() == k) => {
                let at = |v: &Value| {
                    v.get("at")
                        .and_then(Value::as_str)
                        .map(at_order_key)
                        .unwrap_or_default()
                };
                let newer = into
                    .slots
                    .get(&key)
                    .is_none_or(|held| at(&value) > at(held));
                if newer {
                    into.slots.insert(key, value);
                }
            }
            _ => {
                into.slots.entry(key).or_insert(value);
            }
        }
    }
}

/// Merge `{root}/{lorawan|p2p}/cache/*.json` into `dir`, once (node_registry.md §3, R5).
///
/// The old directories are left in place: a rolled-back build reads them, and they are only a
/// cache. The marker is written last, under a lock, so two processes starting together merge
/// once and a crash half-way merges again (the merge is idempotent).
fn merge_legacy(root: &Path, dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join(".merge.lock"))?;
    #[cfg(unix)]
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
    if dir.join(MIGRATED_MARKER).exists() {
        return Ok(());
    }
    let cache = |transport: Transport| Cache {
        dir: root.join(transport.as_str()).join(CACHE_DIR),
        transport,
    };
    let merged = Cache {
        dir: dir.to_path_buf(),
        transport: Transport::P2p,
    };
    let io = |e: Error| std::io::Error::other(e.message().to_string());
    for transport in [Transport::Lorawan, Transport::P2p] {
        let old = cache(transport);
        for id in old.ids().map_err(io)? {
            let Some(doc) = old.read(&id).ok().flatten() else {
                continue;
            };
            let mut into = merged.read(&id).map_err(io)?.unwrap_or_default();
            into.id = id.clone();
            merge_documents(&mut into, doc);
            merged.write_document(&id, &into).map_err(io)?;
        }
    }
    std::fs::write(dir.join(MIGRATED_MARKER), b"")?;
    drop(lock);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cache(tag: &str) -> (Cache, TempDir) {
        let dir = std::env::temp_dir().join(format!(
            "control-radio-cache-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::new(&dir);
        (Cache::new(&store, Transport::Lorawan), TempDir(dir))
    }

    /// #166: a network-server discard leaves a cap the next `set-config` packs into, for a day.
    #[test]
    fn a_payload_cap_holds_for_a_day() {
        let (cache, _dir) = cache("payload-cap");
        let id = "5876070798ec538a";
        cache.note_payload_cap(id, 51, 1_000).unwrap();
        let doc = cache.read(id).unwrap().unwrap();
        assert_eq!(doc.payload_cap(1_000), Some(51));
        assert_eq!(doc.payload_cap(1_000 + PAYLOAD_CAP_TTL_S), Some(51));
        assert_eq!(doc.payload_cap(1_001 + PAYLOAD_CAP_TTL_S), None);
        assert_eq!(CacheDocument::default().payload_cap(1_000), None);
    }

    /// The shipped path, spelled out — the Landlock table and the unit's `ReadWritePaths` name
    /// this directory literally.
    #[test]
    fn the_shipped_layout_is_the_documented_one() {
        let store = Store::at_default_root();
        // One directory for both carriers since the one Node registry (node_registry.md §3).
        for transport in [Transport::Lorawan, Transport::P2p] {
            assert_eq!(
                Cache::new(&store, transport)
                    .path("70b3d57ed80051b2")
                    .expect("valid id")
                    .display()
                    .to_string(),
                "/data/proximos/radio/cache/70b3d57ed80051b2.json"
            );
        }
    }

    /// R5: the per-carrier caches a pre-registry build kept merge into one document per Node,
    /// once. The newer entry of each slot wins, the sequence keeps its high-water mark, and
    /// the old files stay for a rolled-back build.
    #[test]
    fn the_per_carrier_caches_merge_into_one() {
        let dir = std::env::temp_dir().join(format!("cache-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = "5876070000000413";
        for (carrier, at, seq) in [
            ("lorawan", "2026-09-27T11:17:15.204311Z", 40),
            ("p2p", "2026-09-27T11:10:00Z", 58),
        ] {
            let d = dir.join(carrier).join(CACHE_DIR);
            std::fs::create_dir_all(&d).expect("dir");
            let doc = serde_json::json!({
                "id": id,
                "device_info": { "at": at, "value": { "carrier": carrier } },
                "command_seq": seq,
            });
            std::fs::write(d.join(format!("{id}.json")), doc.to_string()).expect("write");
        }
        let store = Store::new(&dir);
        let doc = Cache::new(&store, Transport::P2p)
            .read(id)
            .expect("reads")
            .expect("merged");
        let info = doc.slot(CacheSlot::DeviceInfo).expect("device_info");
        assert_eq!(info.value["carrier"], "lorawan", "the newer entry");
        assert_eq!(doc.slots[COMMAND_SEQ_KEY], 58, "the higher sequence");
        assert!(dir
            .join("p2p")
            .join(CACHE_DIR)
            .join(format!("{id}.json"))
            .exists());

        // Once: a later legacy write is not merged again.
        std::fs::write(
            dir.join("p2p").join(CACHE_DIR).join(format!("{id}.json")),
            r#"{"id":"5876070000000413","command_seq":99}"#,
        )
        .expect("write");
        let doc = Cache::new(&store, Transport::Lorawan)
            .read(id)
            .expect("reads")
            .expect("doc");
        assert_eq!(doc.slots[COMMAND_SEQ_KEY], 58);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_slot_round_trips_through_the_file() {
        let (c, _d) = cache("roundtrip");
        assert_eq!(c.read("aabbccddeeff0011").expect("reads"), None);

        c.merge_write(
            "aabbccddeeff0011",
            CacheSlot::Message,
            "2026-08-27T10:00:00Z",
            serde_json::json!({"voltage": 3.36}),
        )
        .expect("writes");

        let doc = c.read("aabbccddeeff0011").expect("reads").expect("present");
        let entry = doc.slot(CacheSlot::Message).expect("the slot is filled");
        assert_eq!(entry.at, "2026-08-27T10:00:00Z");
        assert_eq!(entry.value, serde_json::json!({"voltage": 3.36}));
        assert_eq!(doc.id, "aabbccddeeff0011");
        assert_eq!(doc.filled(), vec![CacheSlot::Message]);
    }

    /// The five slots are filled by different frames minutes or hours apart: a telemetry
    /// uplink must not erase the config dump a `get-config` exchange spent minutes collecting.
    #[test]
    fn a_write_merges_rather_than_replacing_the_document() {
        let (c, _d) = cache("merge");
        c.merge_write("dev", CacheSlot::Config, "T1", serde_json::json!({"a": 1}))
            .expect("config");
        c.merge_write("dev", CacheSlot::Message, "T2", serde_json::json!({"v": 2}))
            .expect("message");
        c.merge_write("dev", CacheSlot::Message, "T3", serde_json::json!({"v": 3}))
            .expect("newer message");

        let doc = c.read("dev").expect("reads").expect("present");
        assert_eq!(
            doc.slot(CacheSlot::Config).expect("config survives").at,
            "T1"
        );
        let msg = doc.slot(CacheSlot::Message).expect("message");
        assert_eq!(msg.at, "T3", "the newer write wins its own slot");
        assert_eq!(msg.value, serde_json::json!({"v": 3}));
        assert_eq!(doc.filled(), vec![CacheSlot::Message, CacheSlot::Config]);
    }

    /// A downgrade must not silently erase slots the newer build wrote.
    #[test]
    fn unknown_keys_survive_a_merge() {
        let (c, _d) = cache("unknown");
        c.merge_write("dev", CacheSlot::Message, "T1", serde_json::json!(1))
            .expect("write");
        let path = c.path("dev").expect("path");
        let mut doc: Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        doc.insert(
            "something_from_the_future".into(),
            serde_json::json!("keep"),
        );
        std::fs::write(&path, serde_json::to_string(&doc).expect("ser")).expect("write");

        c.merge_write("dev", CacheSlot::Alarm, "T2", serde_json::json!(2))
            .expect("merge");
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(raw.contains("something_from_the_future"), "{raw}");
        assert!(raw.contains("\"alarm\""));
    }

    #[test]
    fn removal_is_idempotent_and_wipe_clears_every_node() {
        let (c, _d) = cache("remove");
        for id in ["a1", "b2", "c3"] {
            c.merge_write(id, CacheSlot::Message, "T", serde_json::json!(1))
                .expect("write");
        }
        assert_eq!(c.ids().expect("ids"), vec!["a1", "b2", "c3"]);

        c.remove("b2").expect("removes");
        c.remove("b2").expect("removing twice is success");
        assert_eq!(c.ids().expect("ids"), vec!["a1", "c3"]);

        c.wipe().expect("wipes");
        assert!(c.ids().expect("ids").is_empty());
        c.wipe().expect("wiping an empty cache is success");
    }

    /// A P2P serial is operator-supplied. `../../lorawan/chirpstack-api-token-admin` as a
    /// "serial" would turn a cache write into a token overwrite.
    #[test]
    fn an_identifier_cannot_escape_the_cache_directory() {
        let (c, _d) = cache("escape");
        for bad in [
            "../../lorawan/chirpstack-api-token-admin",
            "..",
            "a/b",
            "",
            "with space",
            "nul\0byte",
        ] {
            let err = c.path(bad).expect_err("must be refused");
            assert_eq!(err.kind(), Kind::InvalidArgument, "{bad:?}");
            assert!(c.read(bad).is_err(), "{bad:?}");
            assert!(c.remove(bad).is_err(), "{bad:?}");
        }
        // The identifiers that actually occur are fine.
        c.path("70b3d57ed80051b2").expect("a dev_eui");
        c.path("SN-1234_A").expect("a P2P serial");
    }

    /// Absent and unreadable stay different: absent is "no uplink yet", unreadable is a
    /// StorageError an operator has to act on.
    #[test]
    fn a_corrupt_document_is_a_storage_error_not_an_empty_answer() {
        let (c, _d) = cache("corrupt");
        c.merge_write("dev", CacheSlot::Message, "T", serde_json::json!(1))
            .expect("write");
        std::fs::write(c.path("dev").expect("path"), "{not json").expect("corrupt it");

        let err = c.read("dev").expect_err("must surface");
        assert_eq!(err.kind(), Kind::StorageError);
        assert_eq!(err.exit_code(), 40);
    }

    /// The CLI value and the on-disk key are one enum, so they cannot drift.
    #[test]
    fn every_node_get_type_maps_to_a_slot() {
        use crate::cli::NodeGetType;
        let pairs = [
            (NodeGetType::Message, "message"),
            (NodeGetType::DeviceInfo, "device_info"),
            (NodeGetType::Alarm, "alarm"),
            (NodeGetType::Config, "config"),
            (NodeGetType::FullConfig, "full_config"),
        ];
        for (t, key) in pairs {
            assert_eq!(CacheSlot::from(t).as_str(), key);
        }
        assert_eq!(CacheSlot::ALL.len(), pairs.len());
    }

    #[test]
    fn a_sequence_remembers_the_command_it_was_sent_as() {
        let (c, _dir) = cache("sent");
        let first = c
            .allocate_seq("dev", CommandTag::GetConfig)
            .expect("allocates");
        let second = c
            .allocate_seq("dev", CommandTag::GetInfo)
            .expect("allocates");
        let doc = c.read("dev").expect("reads").expect("present");
        assert_eq!(doc.sent_tag(first), Some(CommandTag::GetConfig as u32));
        assert_eq!(doc.sent_tag(second), Some(CommandTag::GetInfo as u32));
        assert_eq!(
            doc.sent_tag(0),
            None,
            "seq 0 is the Node's own, never a command"
        );
        assert_eq!(doc.sent_tag(second + 1), None);
        assert!(doc.filled().is_empty(), "bookkeeping, not a slot");
    }

    /// A re-request under an exchange's original `seq` allocates nothing, and is still recorded.
    #[test]
    fn a_frame_sent_under_an_explicit_seq_is_recorded_too() {
        let (c, _dir) = cache("sent-explicit");
        c.record_sent("dev", 117, CommandTag::GetConfig)
            .expect("records");
        let doc = c.read("dev").expect("reads").expect("present");
        assert_eq!(doc.sent_tag(117), Some(CommandTag::GetConfig as u32));
        assert!(
            doc.slots.get(COMMAND_SEQ_KEY).is_none(),
            "the high-water mark is the allocator's, not touched here"
        );
        assert!(doc.is_continuation(117), "it completes exchange 117");
    }

    /// Q-N1: an allocated seq is not a continuation, and a coalesced one names its successor.
    #[test]
    fn a_superseded_seq_names_the_one_that_replaced_it() {
        let (c, _dir) = cache("superseded");
        let old = c
            .allocate_seq("dev", CommandTag::ClockSync)
            .expect("allocates");
        let new = c
            .allocate_seq("dev", CommandTag::ClockSync)
            .expect("allocates");
        c.note_superseded("dev", old, new).expect("notes");
        c.note_superseded("dev", new + 5, new)
            .expect("an unknown seq is a no-op");
        let doc = c.read("dev").expect("reads").expect("present");
        assert!(!doc.is_continuation(old));
        let sent = doc.slots[SENT_COMMANDS_KEY].as_array().expect("array");
        assert_eq!(sent[0]["superseded_by"], new);
        assert!(sent[1].get("superseded_by").is_none());
        assert_eq!(doc.sent_tag(old), Some(CommandTag::ClockSync as u32));
    }

    /// #129: the queue-item id an enqueue returned leads back to the seq it carried.
    #[test]
    fn a_downlink_id_leads_back_to_its_seq() {
        let (c, _dir) = cache("downlink-id");
        let a = c
            .allocate_seq("dev", CommandTag::GetConfig)
            .expect("allocates");
        let b = c
            .allocate_seq("dev", CommandTag::GetInfo)
            .expect("allocates");
        c.note_downlink_id("dev", a, "item-a").expect("notes");
        c.note_downlink_id("dev", b, "item-b").expect("notes");
        c.note_downlink_id("dev", b + 7, "item-x")
            .expect("an unknown seq is a no-op");
        let doc = c.read("dev").expect("reads").expect("present");
        assert_eq!(doc.seq_for_downlink("item-a"), Some(a));
        assert_eq!(doc.seq_for_downlink("item-b"), Some(b));
        assert_eq!(doc.seq_for_downlink("item-x"), None);
        assert_eq!(doc.seq_for_downlink(""), None);
        assert_eq!(doc.sent_tag(a), Some(CommandTag::GetConfig as u32));
    }

    #[test]
    fn only_the_latest_sent_commands_are_kept() {
        let (c, _dir) = cache("sent-cap");
        let seqs: Vec<u32> = (0..SENT_COMMANDS_KEPT + 3)
            .map(|_| {
                c.allocate_seq("dev", CommandTag::GetConfig)
                    .expect("allocates")
            })
            .collect();
        let doc = c.read("dev").expect("reads").expect("present");
        assert_eq!(doc.sent_tag(seqs[0]), None, "the oldest fell off");
        assert_eq!(
            doc.sent_tag(*seqs.last().expect("some")),
            Some(CommandTag::GetConfig as u32)
        );
        let kept = doc.slots[SENT_COMMANDS_KEY]
            .as_array()
            .expect("array")
            .len();
        assert_eq!(kept, SENT_COMMANDS_KEPT);
    }

    /// Q-N3 (node_reported_state.md §2.2, withdrawn 2026-09-28): `set-config` no longer
    /// records `written` on its entry, but a document an older build wrote still carries it.
    /// It loads, answers as before, and the next write keeps the entry as it found it.
    #[test]
    fn a_document_that_still_carries_written_loads() {
        let (c, _dir) = cache("written");
        std::fs::create_dir_all(c.dir()).expect("dir");
        std::fs::write(
            c.path("dev").expect("path"),
            r#"{ "id": "dev", "command_seq": 22,
                 "sent_commands": [ { "seq": 22, "tag": 2, "written": "22021018" } ] }"#,
        )
        .expect("seed");
        let doc = c.read("dev").expect("reads").expect("present");
        assert_eq!(doc.sent_tag(22), Some(CommandTag::SetParam as u32));
        assert_eq!(
            c.allocate_seq("dev", CommandTag::GetInfo)
                .expect("allocates"),
            23
        );
        let doc = c.read("dev").expect("reads").expect("present");
        assert_eq!(doc.slots[SENT_COMMANDS_KEY][0]["written"], "22021018");
        assert_eq!(doc.sent_tag(23), Some(CommandTag::GetInfo as u32));
    }

    /// Q-N2: a join's session is kept beside the slots, not as one, and a move clears it
    /// without touching what the Node reported.
    #[test]
    fn a_lorawan_session_is_bookkeeping_that_a_clear_drops() {
        let (c, _dir) = cache("session");
        c.clear_lorawan_session("dev")
            .expect("clearing a Node never heard from is success");
        assert_eq!(c.read("dev").expect("reads"), None, "and creates no file");

        let session = LorawanSession {
            dev_addr: "01ab23cd".into(),
            joined_at: "2026-09-27T21:04:11.482161Z".into(),
        };
        c.merge_write("dev", CacheSlot::Message, "T1", serde_json::json!(1))
            .expect("write");
        c.set_lorawan_session("dev", &session).expect("keeps");
        let doc = c.read("dev").expect("reads").expect("present");
        assert_eq!(doc.lorawan_session(), Some(session));
        assert_eq!(
            doc.filled(),
            vec![CacheSlot::Message],
            "not a node-get slot"
        );
        assert_eq!(doc.last_heard().as_deref(), Some("T1"));

        c.clear_lorawan_session("dev").expect("clears");
        let doc = c.read("dev").expect("reads").expect("present");
        assert_eq!(doc.lorawan_session(), None);
        assert!(doc.slot(CacheSlot::Message).is_some(), "the reports stay");
    }

    /// Two processes write the same node's document: the runtime on an uplink, the CLI on a
    /// `node-send`. A shared `{id}.tmp` lets one `File::create` truncate the other's half-written
    /// file and then rename the torn result over the real one.
    #[test]
    fn two_writers_never_share_a_tmp_path() {
        let path = Path::new("/data/proximos/radio/p2p/cache/0011223344556677.json");
        assert_ne!(tmp_path(path, 1), tmp_path(path, 2));
        assert_ne!(tmp_path(path, 1), path.to_path_buf());
    }
}
