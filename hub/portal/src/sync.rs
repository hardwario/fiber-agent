//! Portal side of the Hub configuration pull (plan/portal/config_sync.md
//! §5–§10, decision #43; phase S1).
//!
//! - **Generation.** [`note_save`] runs on every save of a verb the
//!   document carries ([`affects_document`]): it bumps the Hub's `gen`,
//!   remembers who saved the key (the editor the item is re-signed with), and publishes the
//!   unsigned `config/sync/hint` so the Hub asks at once.
//! - **Document.** [`desired_entries`] builds the Hub's desired values from
//!   the Portal's stored copy (`field_thresholds`, decision #41): the wired
//!   lines and the Beacon bands. Node items are phase C ([`node_entries`]).
//!   The 1-Wire read interval comes from its stored copy (`config_desired`).
//!   Nothing else is in the document (§13 Q18).
//! - **Request/ack.** [`SyncService`] answers `config/sync/req` with
//!   `up_to_date` or the full document, each item signed at send time with
//!   its editor's key — or `portal-sync`'s when the editor is gone,
//!   disabled or no longer allowed (§13 Q8) — and records `config/sync/ack`.
//! - **Alarm.** One `sync:<hub>` row per Hub (Q12), system class outside
//!   "Poruchy": online without a request for 30 min, or an item failed for
//!   over an hour; cleared once neither holds.
//!
//! Drift (the Hub reports our `gen` with another hash) is overwritten by
//! the full document and audited, never shown (Q14). The per-save direct
//! relay is gone for every verb the document carries (cut-over, amendment
//! 2026-10-15 B): the Hub ack settles the save on the command status.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use fiber_protocol::certificate::Certificate;
use fiber_protocol::{
    canonical_json, is_desired_verb, parse_sync_topic, DesiredDocument, SyncAck, SyncHint,
    SyncItem, SyncRequest, SyncResponse, SyncTarget, SyncTopic, MAX_RESP_BYTES,
};
use parking_lot::Mutex;
use portal_storage::{ConfigGeneration, ConfigSyncResult, FieldThreshold, NewAlarmEvent};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, mpsc};

use crate::alarms::{AlarmNudge, NotifyJob};
use crate::auth::{AuthService, CurrentUser};
use crate::db::Db;
use crate::keys::{KeyService, UserSigningKey};
use crate::notify::{SystemAlertEvent, SystemAlertKind};
use crate::relay::CommandRelay;
use crate::state::StateStore;

/// `alarm_events.alarm_type` of the sync alarm (`AlarmType::ConfigSync`).
pub const ALARM_TYPE: &str = "CONFIG_SYNC";
/// The alarm's two sentences; `i18n::translate_alarm_message` words them.
pub const MSG_NO_REQUEST: &str = "The Hub has not asked for its configuration for 30 minutes";
pub const MSG_ITEM_FAILED: &str =
    "The Hub failed to apply part of its configuration for over an hour";

/// An online Hub that has not asked for this long is behind (§10).
pub const NO_REQUEST_AFTER_S: i64 = 30 * 60;
/// A failed item older than this raises the alarm (§10).
pub const FAILED_ITEM_AFTER_S: i64 = 60 * 60;
const SWEEP_PERIOD: Duration = Duration::from_secs(60);
/// The same full document (gen + hash) goes to one Hub at most this often;
/// requests in between are dropped, the Hub retries with backoff.
pub const FULL_DOC_MIN_INTERVAL_S: i64 = 30;
/// Pseudo-item key a document too large to send is failed under.
pub const DOCUMENT_SIZE_KEY: &str = "document/size";

/// The verbs the document carries (phase B scope, §13 Q18): the 1-Wire
/// limits and hysteresis, the 1-Wire read interval and the Beacon bands
/// (the delete takes a band out). A save of one bumps `gen` and is delivered
/// by the pull alone; every other verb keeps the direct relay.
pub fn affects_document(command: &str) -> bool {
    in_document(command)
}

/// See [`affects_document`]; the name the cut-over reads.
pub fn in_document(command: &str) -> bool {
    matches!(
        command,
        "set_threshold" | "set_eye_field_threshold" | "delete_eye_field_threshold"
    ) || stored_in_config_desired(command)
}

/// `sync:<hub>` — the alarm row's `sensor_id`.
pub fn alarm_sensor_id(hub: &str) -> String {
    format!("sync:{}", hub.to_ascii_lowercase())
}

/// One desired value before it is signed.
#[derive(Debug, Clone, PartialEq)]
pub struct DesiredEntry {
    pub key: String,
    pub target: SyncTarget,
    pub command: String,
    pub params: Value,
}

/// The document key a relayed command writes, when it has one.
pub fn item_key(command: &str, params: &Value) -> Option<String> {
    let str_of = |k: &str| params.get(k).and_then(Value::as_str);
    match command {
        "set_threshold" => Some(format!("hub/threshold/{}", params.get("line")?.as_u64()?)),
        "set_eye_field_threshold" | "delete_eye_field_threshold" => Some(format!(
            "beacon/{}/threshold/{}",
            str_of("mac")?.to_ascii_uppercase(),
            str_of("field")?
        )),
        "set_sensor_name" | "set_sensor_location" => Some(format!(
            "hub/{}/{}",
            command.trim_start_matches("set_"),
            params.get("line")?.as_u64()?
        )),
        c if is_desired_verb(c) => Some(format!("hub/{}", c.trim_start_matches("set_"))),
        _ => None,
    }
}

/// The desired entries built from the Portal's stored bounds of one Hub.
/// A disabled or incomplete band is not desired state and is left out: the
/// Hub applier treats a band missing from the document as deleted (S3).
/// A wired line's switched-off band is the exception (#167): it is sent with
/// `enabled: false`, since deleting it would hand the line back to the Hub's
/// default band, which grades.
pub fn entries_from_thresholds(rows: &[FieldThreshold]) -> Vec<DesiredEntry> {
    let defaults = crate::threshold_defaults::configured();
    let mut out = Vec::new();
    for row in rows
        .iter()
        .filter(|r| r.enabled || r.scope == portal_storage::THRESHOLD_SCOPE_HUB)
    {
        let h = crate::threshold_defaults::hysteresis(
            &defaults,
            &row.scope,
            &row.field,
            row.hysteresis,
        );
        let bounds = [
            row.critical_low,
            row.warning_low,
            row.warning_high,
            row.critical_high,
        ];
        match row.scope.as_str() {
            portal_storage::THRESHOLD_SCOPE_HUB => {
                let Ok(line) = row.entity.parse::<u8>() else {
                    continue;
                };
                let Ok(params) =
                    crate::routes::hub_routes::wired_threshold_params(line, bounds, h, row.enabled)
                else {
                    continue;
                };
                out.push(DesiredEntry {
                    key: format!("hub/threshold/{line}"),
                    target: SyncTarget::Hub,
                    command: "set_threshold".into(),
                    params,
                });
            }
            portal_storage::THRESHOLD_SCOPE_EYE => {
                let mac = row.entity.to_ascii_uppercase();
                // The shape `beacon_routes` relays (`set_eye_field_threshold`).
                let params = json!({
                    "mac": mac,
                    "field": row.field,
                    "critical_low": row.critical_low,
                    "warning_low": row.warning_low,
                    "warning_high": row.warning_high,
                    "critical_high": row.critical_high,
                    "hysteresis": h,
                });
                out.push(DesiredEntry {
                    key: format!("beacon/{mac}/threshold/{}", row.field),
                    target: SyncTarget::Beacon { mac },
                    command: "set_eye_field_threshold".into(),
                    params,
                });
            }
            _ => {}
        }
    }
    out
}

/// Per-Node sections of the pulled document. Empty by decision: Node
/// delivery is a direct signed command to the home Hub, one coalesced
/// `set_sticker_config` per Node (config_sync.md Q20a, `pending.rs`).
fn node_entries(_hub: &str) -> Vec<DesiredEntry> {
    Vec::new()
}

/// The Hub verbs whose value lives in `config_desired` (V42): the 1-Wire
/// read interval alone (§13 Q18); `set_threshold`'s bounds live in
/// `field_thresholds`.
pub fn stored_in_config_desired(command: &str) -> bool {
    command == "set_sample_interval"
}

/// `config_desired` rows as entries; an unparseable row is skipped.
fn entries_from_stored(rows: Vec<(String, String, String)>) -> Vec<DesiredEntry> {
    rows.into_iter()
        .filter(|(_, command, _)| stored_in_config_desired(command))
        .filter_map(|(key, command, params)| {
            Some(DesiredEntry {
                key,
                target: SyncTarget::Hub,
                command,
                params: serde_json::from_str(&params).ok()?,
            })
        })
        .collect()
}

/// What a Hub's retained `config/state` says for the stored-value keys:
/// the one-off seed of a Hub the Portal holds no value for yet, so its
/// first full document restates the Hub rather than wiping it. Only the
/// read interval alone (§13 Q19), and only a value the Hub would accept.
pub fn seed_rows(device: &crate::state::DeviceState) -> Vec<(String, String, String)> {
    let Some(sample_ms) = device
        .config
        .intervals
        .as_ref()
        .and_then(|i| i.sample_interval_ms)
    else {
        return Vec::new();
    };
    let sample_s = u64::try_from(sample_ms).unwrap_or(0).saturating_add(999) / 1000;
    if !fiber_protocol::config_sync::SAMPLE_INTERVAL_S.contains(&sample_s) {
        return Vec::new();
    }
    let params = json!({ "sample_interval_s": sample_s });
    item_key("set_sample_interval", &params)
        .map(|key| vec![(key, "set_sample_interval".to_string(), params.to_string())])
        .unwrap_or_default()
}

/// Everything the Portal wants this Hub to hold, sorted by key.
pub async fn desired_entries(
    db: &Db,
    hub: &str,
) -> Result<Vec<DesiredEntry>, portal_storage::StorageError> {
    let h = hub.to_string();
    let (rows, stored) = db
        .run(move |s| Ok((s.sync_field_thresholds(&h)?, s.list_config_desired(&h)?)))
        .await?;
    let mut entries = entries_from_thresholds(&rows);
    entries.extend(entries_from_stored(stored));
    entries.extend(node_entries(hub));
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(entries)
}

/// `fiber_protocol::document_hash` over unsigned entries: the same
/// canonical `[{key, command, params}]` sorted by key, so the hash is known
/// before (and without) signing. Pinned equal by a test.
pub fn entries_hash(entries: &[DesiredEntry]) -> String {
    let mut sorted: Vec<&DesiredEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.key.cmp(&b.key));
    let values: Vec<Value> = sorted
        .iter()
        .map(|e| json!({"key": e.key, "command": e.command, "params": e.params}))
        .collect();
    Sha256::digest(canonical_json(&Value::Array(values)).as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// The save hook: bump the Hub's `gen`, remember the key's editor, and
/// publish the hint. Returns the new `gen` and the item key. Errors are
/// logged; a save never fails on sync bookkeeping — the next request
/// recomputes the hash anyway.
pub async fn note_save(
    db: &Db,
    relay: &CommandRelay,
    hub: &str,
    command: &str,
    params: &Value,
    editor: &str,
) -> Option<(u64, Option<String>)> {
    if !affects_document(command) {
        return None;
    }
    let now = now_unix();
    let key = item_key(command, params);
    // The Portal's own copy of a value with no other home (V42), stored
    // before the hash so the document already holds it.
    if let (true, Some(k)) = (stored_in_config_desired(command), &key) {
        let (h, k, c, p) = (
            hub.to_string(),
            k.clone(),
            command.to_string(),
            params.to_string(),
        );
        if let Err(err) = db
            .write(move |s| s.upsert_config_desired(&h, &k, &c, &p, now))
            .await
        {
            tracing::warn!(%err, hub, command, "config sync: stored copy not written");
        }
    }
    let hash = match desired_entries(db, hub).await {
        Ok(entries) => entries_hash(&entries),
        Err(err) => {
            tracing::warn!(%err, hub, "config sync: desired document unreadable");
            String::new()
        }
    };
    let (h, e, k) = (hub.to_string(), editor.to_string(), key.clone());
    let gen = db
        .write(move |s| {
            let gen = s.bump_config_generation(&h, &hash, now)?;
            if let Some(key) = &k {
                s.note_config_sync_item(&h, key, &e, gen, now)?;
            }
            Ok(gen)
        })
        .await;
    match gen {
        Ok(gen) => {
            publish_hint(relay, hub, gen).await;
            Some((gen, key))
        }
        Err(err) => {
            tracing::warn!(%err, hub, command, "config sync: generation not bumped");
            None
        }
    }
}

async fn publish_hint(relay: &CommandRelay, hub: &str, gen: u64) {
    let topic = SyncTopic::Hint.topic(relay.topic_prefix(), hub);
    let body = serde_json::to_vec(&SyncHint { gen }).unwrap_or_default();
    if let Err(err) = relay.publish_raw(topic, body).await {
        tracing::debug!(%err, hub, "config sync hint not published");
    }
}

/// A Hub → Portal sync message handed over by the MQTT ingest.
#[derive(Debug, Clone)]
pub struct SyncInbound {
    pub hub: String,
    pub kind: SyncTopic,
    pub payload: Vec<u8>,
}

/// `Some` when `topic` is a Hub → Portal sync topic.
pub fn inbound(topic: &str, prefix: &str, payload: &[u8]) -> Option<SyncInbound> {
    let (hub, kind) = parse_sync_topic(topic, prefix)?;
    matches!(kind, SyncTopic::Req | SyncTopic::Ack).then(|| SyncInbound {
        hub: hub.to_string(),
        kind,
        payload: payload.to_vec(),
    })
}

/// What the `sync:<hub>` alarm should say right now; `None` = no alarm.
///
/// The no-request rule applies only to a Hub that has asked at least once:
/// until its image carries the sync client (S3), every online Hub would
/// otherwise raise it.
pub fn alarm_message(
    row: &ConfigGeneration,
    online: bool,
    online_since: Option<i64>,
    now: i64,
) -> Option<&'static str> {
    if let (true, Some(last_req)) = (online, row.last_req_at) {
        let quiet_from = last_req.max(online_since.unwrap_or(last_req));
        if now - quiet_from >= NO_REQUEST_AFTER_S {
            return Some(MSG_NO_REQUEST);
        }
    }
    if row
        .failed_since
        .is_some_and(|t| now - t >= FAILED_ITEM_AFTER_S)
    {
        return Some(MSG_ITEM_FAILED);
    }
    None
}

/// `(gen, hash, sent_at)` of the last full document sent to a Hub.
type LastFull = (u64, String, i64);

/// A signing key and whether it is `portal-sync`'s.
type SignerChoice = (UserSigningKey, bool);

/// The request/ack handler and the alarm sweep.
#[derive(Clone)]
pub struct SyncService {
    db: Db,
    keys: KeyService,
    relay: CommandRelay,
    auth: AuthService,
    store: Arc<StateStore>,
    nudge: Option<broadcast::Sender<AlarmNudge>>,
    /// System-alert e-mail of the alarm's onset (`SystemAlerts`, like `cal:`).
    notify_tx: Option<mpsc::UnboundedSender<NotifyJob>>,
    /// When the sweep first saw each Hub online (lowercase hub).
    online_since: Arc<Mutex<HashMap<String, i64>>>,
    /// Last full document sent per Hub (lowercase): `(gen, hash, at)`.
    last_full: Arc<Mutex<HashMap<String, LastFull>>>,
}

impl SyncService {
    pub fn new(
        db: Db,
        keys: KeyService,
        relay: CommandRelay,
        auth: AuthService,
        store: Arc<StateStore>,
        nudge: Option<broadcast::Sender<AlarmNudge>>,
    ) -> Self {
        Self {
            db,
            keys,
            relay,
            auth,
            store,
            nudge,
            notify_tx: None,
            online_since: Arc::new(Mutex::new(HashMap::new())),
            last_full: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Mails the alarm's onset through the notification worker.
    pub fn with_notify(mut self, tx: mpsc::UnboundedSender<NotifyJob>) -> Self {
        self.notify_tx = Some(tx);
        self
    }

    /// Runs the inbound handler and the minute sweep.
    pub fn spawn(self, mut rx: mpsc::UnboundedReceiver<SyncInbound>) {
        let handler = self.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                handler.handle(msg).await;
            }
        });
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SWEEP_PERIOD);
            loop {
                tick.tick().await;
                self.sweep(now_unix()).await;
            }
        });
    }

    pub async fn handle(&self, msg: SyncInbound) {
        match msg.kind {
            SyncTopic::Req => match serde_json::from_slice::<SyncRequest>(&msg.payload) {
                Ok(req) => {
                    let now = now_unix();
                    if let Some(resp) = self.answer(&msg.hub, &req, now).await {
                        self.respond(&msg.hub, resp, now).await;
                    }
                }
                Err(err) => tracing::warn!(%err, hub = msg.hub, "config sync request undecodable"),
            },
            SyncTopic::Ack => match serde_json::from_slice::<SyncAck>(&msg.payload) {
                Ok(ack) => self.record_ack(&msg.hub, &ack, now_unix()).await,
                Err(err) => tracing::warn!(%err, hub = msg.hub, "config sync ack undecodable"),
            },
            _ => {}
        }
    }

    /// Publish one answer, QoS 0, unless it is a full document that was
    /// just sent ([`FULL_DOC_MIN_INTERVAL_S`]) or too large
    /// ([`MAX_RESP_BYTES`]: refused, logged, failed as [`DOCUMENT_SIZE_KEY`]
    /// so the `sync:` alarm reports it). Returns whether it was published.
    pub async fn respond(&self, hub: &str, resp: SyncResponse, now: i64) -> bool {
        if let SyncResponse::Document(doc) = &resp {
            if !self.full_doc_allowed(hub, doc.gen, &doc.hash, now) {
                tracing::debug!(hub, gen = doc.gen, "config sync: same document just sent");
                return false;
            }
        }
        let body = match serde_json::to_vec(&resp) {
            Ok(body) => body,
            Err(err) => {
                tracing::warn!(%err, hub, "config sync response not encodable");
                return false;
            }
        };
        let is_doc = matches!(resp, SyncResponse::Document(_));
        if is_doc {
            let error = (body.len() > MAX_RESP_BYTES).then(|| {
                format!(
                    "document of {} B exceeds the {} B limit",
                    body.len(),
                    MAX_RESP_BYTES
                )
            });
            let (h, e) = (hub.to_string(), error.clone());
            if let Err(err) = self
                .db
                .write(move |s| {
                    s.set_config_sync_item_error(&h, DOCUMENT_SIZE_KEY, e.as_deref(), now)
                })
                .await
            {
                tracing::warn!(%err, hub, "config sync: size state not stored");
            }
            if let Some(error) = error {
                tracing::error!(hub, bytes = body.len(), "config sync: {error}, not sent");
                return false;
            }
        }
        let topic = SyncTopic::Resp.topic(self.relay.topic_prefix(), hub);
        let bytes = body.len();
        match self.relay.publish_raw(topic, body).await {
            Ok(()) => {
                tracing::info!(hub, bytes, full = is_doc, "config sync: response sent");
                true
            }
            Err(err) => {
                tracing::warn!(%err, hub, "config sync response not published");
                false
            }
        }
    }

    /// Rate limit: the same `(gen, hash)` to one Hub at most once per
    /// [`FULL_DOC_MIN_INTERVAL_S`]. Records the send when allowed.
    pub fn full_doc_allowed(&self, hub: &str, gen: u64, hash: &str, now: i64) -> bool {
        let mut last = self.last_full.lock();
        let key = hub.to_ascii_lowercase();
        if let Some((g, h, at)) = last.get(&key) {
            if *g == gen && h == hash && now - at < FULL_DOC_MIN_INTERVAL_S {
                return false;
            }
        }
        last.insert(key, (gen, hash.to_string(), now));
        true
    }

    /// The answer to one request: `up_to_date` when the Hub holds our gen
    /// and hash, the full signed document otherwise. `None` only when the
    /// Portal's own copy could not be read (the Hub asks again).
    pub async fn answer(&self, hub: &str, req: &SyncRequest, now: i64) -> Option<SyncResponse> {
        self.seed_once(hub, now).await;
        let entries = match desired_entries(&self.db, hub).await {
            Ok(entries) => entries,
            Err(err) => {
                tracing::warn!(%err, hub, "config sync: desired document unreadable");
                return None;
            }
        };
        let hash = entries_hash(&entries);
        let (h, hh) = (hub.to_string(), hash.clone());
        let gen = self
            .db
            .write(move |s| {
                s.record_config_sync_req(&h, now)?;
                s.ensure_config_generation(&h, &hh, now)
            })
            .await
            .map_err(|err| tracing::warn!(%err, hub, "config sync: generation not stored"))
            .ok()?;
        if req.gen == gen && req.hash == hash {
            tracing::debug!(hub, gen, "config sync: up to date");
            return Some(SyncResponse::UpToDate { gen });
        }
        if req.gen == gen {
            // Same generation, other values: changed on the Hub. Portal wins (§8, Q14).
            self.auth
                .audit_as(
                    "config_sync_drift",
                    None,
                    Some(hub),
                    Some(json!({"via": "sync", "gen": gen, "hub_hash": req.hash}).to_string()),
                    true,
                )
                .await;
        }
        let items = self.sign(hub, &entries, gen).await;
        let mut doc = DesiredDocument::new(gen, items);
        // Each certificate once, not per item (HIL: 22 items were 27 KB).
        doc.hoist_certificates();
        tracing::info!(
            hub,
            gen_in = req.gen,
            gen_out = gen,
            items = doc.items.len(),
            "config sync: full document"
        );
        Some(SyncResponse::Document(doc))
    }

    /// The first sync of a Hub the Portal holds no stored copy for: fill
    /// the missing keys from its retained config/state (amendment
    /// 2026-10-15), once. Not marked done while the Hub has said nothing
    /// yet (no snapshot or an empty config), so a later request seeds.
    async fn seed_once(&self, hub: &str, now: i64) {
        let h = hub.to_string();
        if self
            .db
            .run(move |s| s.config_seeded(&h))
            .await
            .unwrap_or(true)
        {
            return;
        }
        let Some(device) = self.store.snapshot_device(hub) else {
            return;
        };
        let rows = seed_rows(&device);
        if rows.is_empty() {
            return;
        }
        let h = hub.to_string();
        match self
            .db
            .write(move |s| s.seed_config_desired(&h, &rows, now))
            .await
        {
            Ok(added) => tracing::info!(hub, added, "config sync: seeded from the Hub's state"),
            Err(err) => tracing::warn!(%err, hub, "config sync: seed not stored"),
        }
    }

    /// Signs every entry with its editor's key (§6), or `portal-sync`'s.
    /// An entry that cannot be signed at all (Hub not paired) is left out
    /// and logged — the hash then differs and the Hub asks again.
    async fn sign(&self, hub: &str, entries: &[DesiredEntry], gen: u64) -> Vec<SyncItem> {
        let h = hub.to_string();
        let editors: HashMap<String, Option<String>> = self
            .db
            .run(move |s| s.config_sync_editors(&h))
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();
        let host = self
            .store
            .snapshot_device(hub)
            .map(|d| d.hostname)
            .unwrap_or_else(|| hub.to_string());
        let mut keys: HashMap<(Option<String>, String), Option<SignerChoice>> = HashMap::new();
        let mut out = Vec::new();
        for entry in entries {
            let editor = editors.get(&entry.key).cloned().flatten();
            let cache = (editor.clone(), entry.command.clone());
            if !keys.contains_key(&cache) {
                let key = self
                    .signing_key(&host, editor.as_deref(), &entry.command)
                    .await;
                keys.insert(cache.clone(), key);
            }
            let Some((key, service)) = keys.get(&cache).and_then(Option::as_ref) else {
                continue;
            };
            let Some(item) = sign_entry(entry, key, editor.as_deref()) else {
                tracing::warn!(hub, key = entry.key, "config sync: item not signed");
                continue;
            };
            let mut details =
                json!({"via": "sync", "gen": gen, "key": entry.key, "command": entry.command});
            if *service {
                details["signed_by"] = json!(crate::keys::PORTAL_SYNC_SIGNER);
            }
            self.auth
                .audit_as(
                    "config_sync_sent",
                    Some(&item.editor),
                    Some(hub),
                    Some(details.to_string()),
                    true,
                )
                .await;
            out.push(item);
        }
        out
    }

    /// The editor's key when the editor still exists, is enabled and may
    /// run `command`; `portal-sync`'s otherwise (`true` = service key).
    async fn signing_key(
        &self,
        host: &str,
        editor: Option<&str>,
        command: &str,
    ) -> Option<(UserSigningKey, bool)> {
        if let Some(user) = self.editor_user(editor).await {
            let permissions = crate::perms::permissions_for(&user);
            let allowed = fiber_protocol::lookup(command).is_some_and(|spec| {
                permissions.iter().any(|p| p == spec.permission)
                    && spec.level <= crate::perms::level_ceiling(&user)
            });
            if allowed {
                match self.keys.ensure_user_key(&user, host, &permissions).await {
                    Ok(key) => return Some((key, false)),
                    Err(err) => tracing::warn!(%err, host, "config sync: editor key unavailable"),
                }
            }
        }
        match self.keys.ensure_sync_service_key(host).await {
            Ok(key) => Some((key, true)),
            Err(err) => {
                tracing::warn!(%err, host, "config sync: portal-sync key unavailable");
                None
            }
        }
    }

    async fn editor_user(&self, editor: Option<&str>) -> Option<CurrentUser> {
        let name = editor?.to_string();
        let user = self
            .db
            .run(move |s| s.get_user_by_username(&name))
            .await
            .ok()??;
        if !user.enabled {
            return None;
        }
        Some(CurrentUser {
            id: user.id,
            username: user.username,
            full_name: user.full_name,
            user_role: user.user_role.parse().ok()?,
            is_superuser: user.is_superuser,
            monitoring_view_mode: user.monitoring_view_mode,
        })
    }

    /// Stores the Hub's ack; failed keys are logged (and feed the alarm).
    pub async fn record_ack(&self, hub: &str, ack: &SyncAck, now: i64) {
        let results: Vec<ConfigSyncResult> = ack
            .results
            .iter()
            .map(|r| ConfigSyncResult {
                key: r.key.clone(),
                ok: r.ok,
                error: r.error.clone(),
            })
            .collect();
        for r in results.iter().filter(|r| !r.ok) {
            tracing::warn!(
                hub,
                gen = ack.gen,
                key = r.key,
                error = r.error.as_deref().unwrap_or(""),
                "config sync: Hub failed an item"
            );
        }
        // Cut-over: the saves this ack covers settle on the command status.
        let failed: HashMap<String, String> = results
            .iter()
            .filter(|r| !r.ok)
            .map(|r| (r.key.clone(), r.error.clone().unwrap_or_default()))
            .collect();
        self.relay.settle_sync_saves(hub, ack.gen, &failed);
        let (h, hash, gen) = (hub.to_string(), ack.hash.clone(), ack.gen);
        if let Err(err) = self
            .db
            .write(move |s| s.record_config_sync_ack(&h, gen, &hash, &results, now))
            .await
        {
            tracing::warn!(%err, hub, "config sync ack not stored");
        }
        self.sweep_hub(hub, now).await;
    }

    /// Brings every Hub's `sync:` alarm in line with its rows.
    pub async fn sweep(&self, now: i64) {
        let snapshot = self.store.snapshot();
        {
            let mut since = self.online_since.lock();
            since.retain(|hub, _| snapshot.device(hub).is_some_and(|d| d.online));
            for d in snapshot.devices.values().filter(|d| d.online) {
                since.entry(d.hostname.to_ascii_lowercase()).or_insert(now);
            }
        }
        let rows = self
            .db
            .run(|s| s.list_config_generations())
            .await
            .unwrap_or_default();
        for row in rows {
            self.apply_alarm(&row, now).await;
        }
    }

    async fn sweep_hub(&self, hub: &str, now: i64) {
        let h = hub.to_string();
        if let Ok(Some(row)) = self.db.run(move |s| s.config_generation(&h)).await {
            self.apply_alarm(&row, now).await;
        }
    }

    async fn apply_alarm(&self, row: &ConfigGeneration, now: i64) {
        let device = self.store.snapshot_device(&row.hub_id);
        let online = device.as_ref().is_some_and(|d| d.online);
        let since = self.online_since.lock().get(&row.hub_id).copied();
        let want = alarm_message(row, online, since, now);
        let hostname = device
            .map(|d| d.hostname)
            .unwrap_or_else(|| row.hub_id.clone());
        let sensor_id = alarm_sensor_id(&row.hub_id);
        let at = crate::calibration::rfc3339(now);
        let host = hostname.clone();
        let changed = self
            .db
            .write(move |s| {
                let open = s.active_alarm(&host, &sensor_id)?;
                Ok(match (want, open) {
                    (None, Some(open)) => s.clear_alarm(&open.id, &at, None)?.then_some(None),
                    (Some(message), None) => {
                        s.insert_alarm_event(&NewAlarmEvent {
                            hostname: host.clone(),
                            sensor_id: sensor_id.clone(),
                            sensor_name: Some(host.clone()),
                            sensor_location: None,
                            alarm_type: ALARM_TYPE.to_string(),
                            severity: "warning".to_string(),
                            from_state: Some("NORMAL".to_string()),
                            to_state: Some("WARNING".to_string()),
                            message: Some(message.to_string()),
                            value: None,
                            threshold: None,
                            triggered_at: at.clone(),
                            cleared_at: None,
                            acknowledged_at: None,
                            acknowledged_by_user_id: None,
                            acknowledged_by_username: None,
                            native_key: None,
                        })?;
                        Some(Some(message))
                    }
                    _ => None,
                })
            })
            .await;
        match changed {
            Ok(Some(opened)) => {
                // Mailed on onset only, like every other system alert (calibration's included).
                if let (Some(message), Some(tx)) = (opened, &self.notify_tx) {
                    let msg_key = if message == MSG_NO_REQUEST {
                        "config_sync_no_request"
                    } else {
                        "config_sync_item_failed"
                    };
                    let _ = tx.send(NotifyJob::SystemAlerts {
                        hostname: hostname.clone(),
                        events: vec![SystemAlertEvent::new(
                            SystemAlertKind::ConfigSyncBehind,
                            format!("Hub {hostname}: {message}"),
                            msg_key,
                            vec![("name", hostname.clone())],
                        )],
                    });
                }
                if let Some(tx) = &self.nudge {
                    let _ = tx.send(AlarmNudge {
                        hostname,
                        activated_ids: Vec::new(),
                        escalated_ids: Vec::new(),
                    });
                }
            }
            Ok(None) => {}
            Err(err) => tracing::warn!(%err, hub = row.hub_id, "sync alarm write failed"),
        }
    }
}

/// One entry, signed with `key`. `editor` is the audit actor carried on
/// the item: the editor even when `portal-sync` signs (§13 Q8).
fn sign_entry(
    entry: &DesiredEntry,
    key: &UserSigningKey,
    editor: Option<&str>,
) -> Option<SyncItem> {
    let certificate = serde_json::from_value::<Certificate>(key.certificate.clone()).ok();
    let request = fiber_protocol::build_request(
        &entry.command,
        entry.params.clone(),
        None,
        &key.signer_id,
        &key.seed,
        certificate.as_ref(),
    )?;
    Some(SyncItem {
        key: entry.key.clone(),
        target: entry.target.clone(),
        command: entry.command.clone(),
        request,
        editor: editor
            .unwrap_or(crate::keys::PORTAL_SYNC_SIGNER)
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::test_support::test_state;
    use fiber_protocol::{document_hash, SyncItemResult};
    use portal_storage::NewFieldThreshold;

    const HUB: &str = "hub-a";

    fn band(scope: &str, entity: &str, field: &str, enabled: bool) -> NewFieldThreshold {
        NewFieldThreshold {
            hostname: HUB.into(),
            scope: scope.into(),
            entity: entity.into(),
            field: field.into(),
            critical_low: Some(-10.0),
            warning_low: Some(0.0),
            warning_high: Some(20.0),
            critical_high: Some(30.0),
            enabled,
        }
    }

    fn row(b: &NewFieldThreshold) -> FieldThreshold {
        FieldThreshold {
            hostname: b.hostname.clone(),
            scope: b.scope.clone(),
            entity: b.entity.clone(),
            field: b.field.clone(),
            critical_low: b.critical_low,
            warning_low: b.warning_low,
            warning_high: b.warning_high,
            critical_high: b.critical_high,
            enabled: b.enabled,
            updated_at: None,
            hysteresis: Some(0.5),
        }
    }

    fn gen_row(last_req_at: Option<i64>, failed_since: Option<i64>) -> ConfigGeneration {
        ConfigGeneration {
            hub_id: HUB.into(),
            gen: 1,
            hash: String::new(),
            changed_at: 0,
            acked_gen: None,
            acked_hash: None,
            acked_at: None,
            last_req_at,
            failed_since,
        }
    }

    #[test]
    fn item_keys_name_the_target() {
        assert_eq!(
            item_key("set_threshold", &json!({"line": 3})).as_deref(),
            Some("hub/threshold/3")
        );
        assert_eq!(
            item_key(
                "delete_eye_field_threshold",
                &json!({"mac": "aa:bb", "field": "temperature"})
            )
            .as_deref(),
            Some("beacon/AA:BB/threshold/temperature")
        );
        assert_eq!(
            item_key("set_screen_brightness", &json!({"value": 5})).as_deref(),
            Some("hub/screen_brightness")
        );
        assert_eq!(item_key("reboot", &json!({})), None);
        assert!(affects_document("delete_eye_field_threshold"));
        assert!(!affects_document("set_network_config"));
    }

    #[test]
    fn the_document_holds_hub_lines_and_enabled_beacon_bands() {
        let rows = [
            row(&band("hub", "2", "temperature", true)),
            row(&band("hub", "3", "temperature", false)),
            row(&band("eye", "aa:bb:cc:dd:ee:01", "humidity", true)),
            row(&band("node", "0011", "temperature", true)),
        ];
        let mut entries = entries_from_thresholds(&rows);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].key, "hub/threshold/2");
        assert_eq!(entries[0].command, "set_threshold");
        assert_eq!(entries[0].params["line"], 2);
        assert_eq!(entries[0].params["thresholds"]["critical_high"], 30.0);
        assert_eq!(entries[0].params["thresholds"]["hysteresis"], 0.5);
        assert_eq!(entries[0].params["thresholds"]["enabled"], true);
        // #167: a switched-off wired line stays in the document, switched off, bounds kept;
        // dropping it would hand the line back to the Hub's default band, which grades.
        let off = entries.remove(1);
        assert_eq!(off.key, "hub/threshold/3");
        assert_eq!(off.params["thresholds"]["enabled"], false);
        assert_eq!(off.params["thresholds"]["critical_high"], 30.0);
        assert_eq!(
            entries[1].key,
            "beacon/AA:BB:CC:DD:EE:01/threshold/humidity"
        );
        assert_eq!(
            entries[1].target,
            SyncTarget::Beacon {
                mac: "AA:BB:CC:DD:EE:01".into()
            }
        );
        assert_eq!(entries[1].command, "set_eye_field_threshold");
    }

    #[test]
    fn the_unsigned_hash_is_the_protocol_hash() {
        let entries = entries_from_thresholds(&[
            row(&band("hub", "0", "temperature", true)),
            row(&band("eye", "AA:BB:CC:DD:EE:01", "temperature", true)),
        ]);
        let items: Vec<SyncItem> = entries
            .iter()
            .map(|e| SyncItem {
                key: e.key.clone(),
                target: e.target.clone(),
                command: e.command.clone(),
                request: fiber_protocol::build_request(
                    &e.command,
                    e.params.clone(),
                    None,
                    "alice",
                    &[1; 32],
                    None,
                )
                .unwrap(),
                editor: "alice".into(),
            })
            .rev()
            .collect();
        assert_eq!(entries_hash(&entries), document_hash(&items));
    }

    #[test]
    fn the_alarm_rules() {
        let now = 100_000;
        // Never asked (Hub without the sync client yet): no alarm.
        assert_eq!(
            alarm_message(&gen_row(None, None), true, Some(0), now),
            None
        );
        // Online, last request 31 min ago.
        let stale = gen_row(Some(now - 31 * 60), None);
        assert_eq!(
            alarm_message(&stale, true, Some(0), now),
            Some(MSG_NO_REQUEST)
        );
        // Offline, or only just back online: not behind yet.
        assert_eq!(alarm_message(&stale, false, None, now), None);
        assert_eq!(alarm_message(&stale, true, Some(now - 60), now), None);
        // A failed item: alarm only after an hour.
        let fresh = Some(now - 60);
        assert_eq!(
            alarm_message(&gen_row(fresh, Some(now - 600)), true, None, now),
            None
        );
        assert_eq!(
            alarm_message(&gen_row(fresh, Some(now - 3601)), true, None, now),
            Some(MSG_ITEM_FAILED)
        );
    }

    async fn setup() -> (crate::http::AppState, tempfile::TempDir, SyncService) {
        let (state, dir) = test_state().await;
        state
            .keys
            .store_pairing(
                HUB,
                Some("test-ca".into()),
                "00".repeat(32),
                [7u8; 32],
                None,
                None,
            )
            .await
            .expect("pair");
        for b in [
            band("hub", "1", "temperature", true),
            band("eye", "AA:BB:CC:DD:EE:01", "temperature", true),
        ] {
            state
                .db
                .write(move |s| s.upsert_field_threshold(&b, Some("admin")))
                .await
                .expect("band");
        }
        let svc = SyncService::new(
            state.db.clone(),
            state.keys.clone(),
            state.relay.clone(),
            state.auth.clone(),
            Arc::clone(&state.store),
            None,
        );
        (state, dir, svc)
    }

    #[tokio::test]
    async fn a_save_bumps_the_generation_and_hints_the_hub() {
        let (state, _dir, _svc) = setup().await;
        let params = json!({"line": 1});
        note_save(
            &state.db,
            &state.relay,
            HUB,
            "set_threshold",
            &params,
            "admin",
        )
        .await;
        note_save(&state.db, &state.relay, HUB, "reboot", &params, "admin").await;
        note_save(
            &state.db,
            &state.relay,
            HUB,
            "set_threshold",
            &params,
            "admin",
        )
        .await;
        let row = state
            .db
            .run(|s| s.config_generation(HUB))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.gen, 2, "one-shot verbs do not count");
        let entries = desired_entries(&state.db, HUB).await.unwrap();
        assert_eq!(row.hash, entries_hash(&entries));
        let hints: Vec<_> = state
            .relay
            .published()
            .into_iter()
            .filter(|(t, _)| t.ends_with("/hub-a/config/sync/hint"))
            .map(|(_, body)| body["gen"].as_u64())
            .collect();
        assert_eq!(hints, vec![Some(1), Some(2)]);
        let editors = state.db.run(|s| s.config_sync_editors(HUB)).await.unwrap();
        assert_eq!(
            editors,
            vec![("hub/threshold/1".into(), Some("admin".into()))]
        );
    }

    fn doc_with_note(gen: u64, note_len: usize) -> DesiredDocument {
        let request = fiber_protocol::build_request(
            "set_sensor_name",
            json!({"line": 1, "name": "x".repeat(note_len)}),
            None,
            "admin",
            &[7; 32],
            None,
        )
        .unwrap();
        DesiredDocument::new(
            gen,
            vec![SyncItem {
                key: "hub/sensor_name/1".into(),
                target: SyncTarget::Hub,
                command: "set_sensor_name".into(),
                request,
                editor: "admin".into(),
            }],
        )
    }

    fn resp_count(state: &crate::http::AppState) -> usize {
        state
            .relay
            .published()
            .iter()
            .filter(|(t, _)| t.ends_with("/config/sync/resp"))
            .count()
    }

    #[tokio::test]
    async fn the_same_full_document_is_rate_limited_per_hub() {
        let (state, _dir, svc) = setup().await;
        let doc = doc_with_note(3, 4);
        assert!(
            svc.respond(HUB, SyncResponse::Document(doc.clone()), 100)
                .await
        );
        assert!(
            !svc.respond(HUB, SyncResponse::Document(doc.clone()), 110)
                .await
        );
        // up_to_date is never throttled; another Hub is independent.
        assert!(
            svc.respond(HUB, SyncResponse::UpToDate { gen: 3 }, 111)
                .await
        );
        assert!(
            svc.respond("hub-b", SyncResponse::Document(doc.clone()), 112)
                .await
        );
        // A new generation goes at once; the old one again after the interval.
        assert!(
            svc.respond(HUB, SyncResponse::Document(doc_with_note(4, 4)), 113)
                .await
        );
        assert!(
            svc.respond(
                HUB,
                SyncResponse::Document(doc),
                113 + FULL_DOC_MIN_INTERVAL_S
            )
            .await
        );
        assert_eq!(resp_count(&state), 5);
    }

    #[tokio::test]
    async fn an_oversized_document_is_not_sent_and_fails_the_hub() {
        let (state, _dir, svc) = setup().await;
        // A Hub row exists once it has asked.
        state
            .db
            .write(|s| s.record_config_sync_req(HUB, 50))
            .await
            .unwrap();
        let big = doc_with_note(2, MAX_RESP_BYTES + 1);
        assert!(!svc.respond(HUB, SyncResponse::Document(big), 100).await);
        assert_eq!(resp_count(&state), 0);
        let row = state
            .db
            .run(|s| s.config_generation(HUB))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.failed_since, Some(100));
        assert_eq!(
            alarm_message(&row, false, None, 100 + FAILED_ITEM_AFTER_S),
            Some(MSG_ITEM_FAILED)
        );
        assert!(
            svc.respond(HUB, SyncResponse::Document(doc_with_note(3, 4)), 200)
                .await
        );
        let row = state
            .db
            .run(|s| s.config_generation(HUB))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.failed_since, None, "a document that fits clears it");
    }

    #[tokio::test]
    async fn a_request_gets_the_document_then_up_to_date() {
        let (state, _dir, svc) = setup().await;
        note_save(
            &state.db,
            &state.relay,
            HUB,
            "set_threshold",
            &json!({"line": 1}),
            "admin",
        )
        .await;
        // The Beacon band was saved by an account that no longer exists.
        let (h, k) = (
            HUB.to_string(),
            "beacon/AA:BB:CC:DD:EE:01/threshold/temperature",
        );
        state
            .db
            .write(move |s| s.note_config_sync_item(&h, k, "ghost", 1, 0))
            .await
            .unwrap();

        let fresh = SyncRequest {
            gen: 0,
            hash: String::new(),
            boot_id: "b".into(),
        };
        let Some(SyncResponse::Document(doc)) = svc.answer(HUB, &fresh, 1_000).await else {
            panic!("expected a document");
        };
        assert_eq!(doc.items.len(), 2);
        assert_eq!(doc.hash, document_hash(&doc.items));
        let by_key: HashMap<_, _> = doc.items.iter().map(|i| (i.key.as_str(), i)).collect();
        let line = by_key["hub/threshold/1"];
        assert_eq!(line.editor, "admin");
        assert!(line.request.signer_id.starts_with("user-"));
        let band = by_key[k];
        assert_eq!(band.editor, "ghost");
        assert_eq!(band.request.signer_id, crate::keys::PORTAL_SYNC_SIGNER);
        assert_eq!(band.command, band.request.command_type);

        let current = SyncRequest {
            gen: doc.gen,
            hash: doc.hash.clone(),
            boot_id: "b".into(),
        };
        assert!(matches!(
            svc.answer(HUB, &current, 1_001).await,
            Some(SyncResponse::UpToDate { gen }) if gen == doc.gen
        ));
        // Drift: same gen, other hash → the full document again.
        let drift = SyncRequest {
            gen: doc.gen,
            hash: "x".into(),
            boot_id: "b".into(),
        };
        assert!(matches!(
            svc.answer(HUB, &drift, 1_002).await,
            Some(SyncResponse::Document(_))
        ));
        let row = state
            .db
            .run(|s| s.config_generation(HUB))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.last_req_at, Some(1_002));
    }

    #[tokio::test]
    async fn a_failed_item_raises_the_alarm_and_success_clears_it() {
        let (state, _dir, svc) = setup().await;
        let ack = |ok: bool| SyncAck {
            gen: 1,
            hash: "h".into(),
            results: vec![SyncItemResult {
                key: "hub/threshold/1".into(),
                ok,
                error: (!ok).then(|| "rejected".into()),
            }],
        };
        let open = |state: &crate::http::AppState| {
            let db = state.db.clone();
            async move { db.run(|s| s.active_alarm(HUB, "sync:hub-a")).await.unwrap() }
        };
        svc.record_ack(HUB, &ack(false), 1_000).await;
        assert!(open(&state).await.is_none(), "not before an hour");
        svc.sweep(1_000 + FAILED_ITEM_AFTER_S).await;
        let alarm = open(&state).await.expect("sync alarm open");
        assert_eq!(alarm.alarm_type, ALARM_TYPE);
        assert_eq!(alarm.message.as_deref(), Some(MSG_ITEM_FAILED));
        svc.record_ack(HUB, &ack(true), 5_000).await;
        assert!(open(&state).await.is_none(), "cleared on success");
    }

    fn report_config(state: &crate::http::AppState) {
        use fiber_protocol::payloads::config_state::{ConfigStateMsg, Intervals};
        state.store.apply(crate::state::IngestEvent::ConfigState {
            hostname: HUB.to_string(),
            payload: ConfigStateMsg {
                led_brightness: Some(3),
                device_label: Some("Cold room".into()),
                intervals: Some(Intervals {
                    sample_interval_ms: Some(2_000),
                    aggregation_interval_ms: Some(600_000),
                    report_interval_ms: Some(60_000),
                }),
                ..Default::default()
            },
        });
    }

    fn interval(sample_s: u64) -> Value {
        json!({ "sample_interval_s": sample_s })
    }

    /// §13 Q18: the read interval is an item from its stored copy; the
    /// other Hub verbs stay out of the document and do not move `gen`.
    #[tokio::test]
    async fn only_the_scoped_verbs_are_document_items() {
        let (state, _dir, svc) = setup().await;
        note_save(
            &state.db,
            &state.relay,
            HUB,
            "set_sample_interval",
            &interval(5),
            "admin",
        )
        .await;
        for (verb, params) in [
            ("set_led_brightness", json!({"brightness": 7})),
            ("set_sensor_name", json!({"line": 2, "name": "Fridge"})),
            ("set_device_label", json!({"label": "x"})),
        ] {
            assert!(!in_document(verb));
            assert!(
                note_save(&state.db, &state.relay, HUB, verb, &params, "admin")
                    .await
                    .is_none()
            );
        }
        let fresh = SyncRequest {
            gen: 0,
            hash: String::new(),
            boot_id: "b".into(),
        };
        let Some(SyncResponse::Document(doc)) = svc.answer(HUB, &fresh, 1_000).await else {
            panic!("expected a document");
        };
        assert_eq!(doc.gen, 1, "only the interval moved gen");
        // setup() stores a wired line and a Beacon band; look at the Hub settings.
        let keys: Vec<&str> = doc
            .items
            .iter()
            .map(|i| i.key.as_str())
            .filter(|k| k.starts_with("hub/") && !k.starts_with("hub/threshold/"))
            .collect();
        assert_eq!(keys, vec!["hub/sample_interval"]);
        let item = doc
            .items
            .iter()
            .find(|i| i.key == "hub/sample_interval")
            .unwrap();
        assert_eq!(item.request.params, interval(5), "no aggregation (Q19)");
        assert_eq!(item.editor, "admin");
    }

    #[tokio::test]
    async fn the_first_sync_seeds_the_interval_without_overriding() {
        let (state, _dir, svc) = setup().await;
        let fresh = SyncRequest {
            gen: 0,
            hash: String::new(),
            boot_id: "b".into(),
        };
        // Nothing reported yet: no seed, and a later request still seeds.
        svc.answer(HUB, &fresh, 900).await;
        assert!(!state.db.run(|s| s.config_seeded(HUB)).await.unwrap());

        report_config(&state);
        let Some(SyncResponse::Document(doc)) = svc.answer(HUB, &fresh, 1_000).await else {
            panic!("expected a document");
        };
        // setup() stores a wired line and a Beacon band; look at the Hub settings.
        let keys: Vec<&str> = doc
            .items
            .iter()
            .map(|i| i.key.as_str())
            .filter(|k| k.starts_with("hub/") && !k.starts_with("hub/threshold/"))
            .collect();
        assert_eq!(
            keys,
            vec!["hub/sample_interval"],
            "only the interval is seeded"
        );
        let item = doc
            .items
            .iter()
            .find(|i| i.key == "hub/sample_interval")
            .unwrap();
        assert_eq!(item.request.params, interval(2));
        assert_eq!(item.request.signer_id, crate::keys::PORTAL_SYNC_SIGNER);
        assert!(state.db.run(|s| s.config_seeded(HUB)).await.unwrap());

        // A stored value is never overridden by a seed.
        let (state, _dir, svc) = setup().await;
        note_save(
            &state.db,
            &state.relay,
            HUB,
            "set_sample_interval",
            &interval(9),
            "admin",
        )
        .await;
        report_config(&state);
        let Some(SyncResponse::Document(doc)) = svc.answer(HUB, &fresh, 1_000).await else {
            panic!("expected a document");
        };
        let item = doc
            .items
            .iter()
            .find(|i| i.key == "hub/sample_interval")
            .unwrap();
        assert_eq!(item.request.params["sample_interval_s"], 9, "stored wins");
    }

    #[tokio::test]
    async fn the_alarm_onset_is_mailed_once_and_the_clear_is_not() {
        let (state, _dir, svc) = setup().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let svc = svc.with_notify(tx);
        let ack = |ok: bool| SyncAck {
            gen: 1,
            hash: "h".into(),
            results: vec![SyncItemResult {
                key: "hub/threshold/1".into(),
                ok,
                error: (!ok).then(|| "rejected".into()),
            }],
        };
        svc.record_ack(HUB, &ack(false), 1_000).await;
        svc.sweep(1_000 + FAILED_ITEM_AFTER_S).await;
        svc.sweep(1_000 + FAILED_ITEM_AFTER_S + 60).await;
        let mut kinds = Vec::new();
        while let Ok(job) = rx.try_recv() {
            if let NotifyJob::SystemAlerts { events, hostname } = job {
                assert_eq!(hostname, HUB);
                kinds.extend(events.into_iter().map(|e| (e.kind, e.key)));
            }
        }
        assert_eq!(
            kinds,
            vec![(SystemAlertKind::ConfigSyncBehind, "config_sync_item_failed")]
        );
        svc.record_ack(HUB, &ack(true), 9_000).await;
        assert!(rx.try_recv().is_err(), "no mail on clear");
        let _ = state;
    }

    #[tokio::test]
    async fn the_hubs_ack_settles_the_tracked_saves() {
        let (state, _dir, svc) = setup().await;
        let mut rx = state.relay.subscribe_status();
        state
            .relay
            .track_sync_save(HUB, "hub/sample_interval", "r1", "set_sample_interval", 2);
        state
            .relay
            .track_sync_save(HUB, "hub/threshold/1", "r2", "set_threshold", 2);
        state
            .relay
            .track_sync_save(HUB, "hub/threshold/2", "r3", "set_threshold", 3);
        let ack = SyncAck {
            gen: 2,
            hash: "h".into(),
            results: vec![SyncItemResult {
                key: "hub/threshold/1".into(),
                ok: false,
                error: Some("out of range".into()),
            }],
        };
        svc.record_ack(HUB, &ack, 1_000).await;
        let mut got = HashMap::new();
        while let Ok(st) = rx.try_recv() {
            got.insert(st.request_id, (st.status, st.message));
        }
        assert_eq!(got["r1"].0, "delivered");
        assert_eq!(got["r2"], ("stored", Some("out of range".to_string())));
        assert!(!got.contains_key("r3"), "gen 3 is not acked yet");
    }
}
