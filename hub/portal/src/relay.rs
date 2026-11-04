//! The signed-command relay (M4-C). `CommandRelay` is the single place that
//! turns a UI-triggered command into wire traffic and turns the device's
//! side of the challenge/confirm/pairing handshakes back into a result the
//! UI can render — see plan/v1/viewer/mqtt.md §"Challenge / confirmation"
//! and §"Pairing".
//!
//! # Ordering guarantee
//!
//! mqtt.md: "the pending request is stored in state **before** publishing,
//! because the device's challenge can arrive before an after-publish write
//! lands... `firmware_export.py` violates this (publish-then-store) —
//! latent race." [`CommandRelay::execute`]'s `ConfigRequest` arm stores the
//! [`PendingCommand`] synchronously, before the `.await` on
//! [`Publisher::publish`] — see `execute_stores_pending_before_publish`
//! below for a test that pins this ordering rather than trusting the
//! comment.
//!
//! # No interactive fallback (phase 1)
//!
//! v1's challenge precedence is bootstrap auto-confirm -> orchestrated
//! claim -> interactive WS round-trip through the browser. Phase 1 only
//! implements the orchestrated path (a command initiated by `execute`
//! always carries its own signer key, so every challenge this relay will
//! ever see either matches a `PendingCommand` it registered itself, or it
//! doesn't and there is nothing further to fall back to). An unmatched
//! challenge is logged and dropped — see `handle_challenge`.
//!
//! # Testability
//!
//! [`Publisher`] is the seam: production wires [`crate::mqtt::MqttHandle`]
//! (which really talks to the broker); tests wire a capturing/blocking
//! double so the pending-map/ordering/timeout assertions below don't need a
//! live broker.
//!
//! Every route calls `execute`, the alarm ack and backfill call `dispatch`,
//! pairing calls `pair`; the `#![allow(dead_code)]` that sat here from the
//! days when nothing did was removed in the 2026-09-15 second review, so
//! the compiler can see this module again.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fiber_protocol::certificate::Certificate;
use fiber_protocol::payloads::config_challenge::ConfigChallengeMsg;
use fiber_protocol::payloads::config_response::{ConfigResponseMsg, ResponseResult};
use fiber_protocol::payloads::errors::DeviceErrorMsg;
use fiber_protocol::payloads::pair_response::PairResponseMsg;
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use zeroize::Zeroizing;

use crate::keys::UserSigningKey;
use crate::mqtt::MqttHandle;
use crate::state::IngestEvent;

/// 20s default timeout for a signed command's full challenge/confirm/
/// response round trip — matches v1's orchestration-registry default
/// (mqtt.md §"Challenge / confirmation").
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
/// 30s pairing timeout — matches v1's `POST /api/auth/devices/{h}/pair`
/// (mqtt.md §"Pairing": "30 s timeout -> HTTP 408").
const PAIR_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a `challenge_id` stays in the seen-set — the device's own
/// challenge timeout, so an id is remembered exactly as long as it could
/// still be answered (design.md §6.2).
const CHALLENGE_TTL: Duration = Duration::from_secs(300);
/// Status broadcast capacity: generous relative to the single-device phase-1
/// UI; a lagging subscriber just misses an intermediate transition (same
/// reasoning as `StateStore`'s `ChangeEvent` channel).
const STATUS_CHANNEL_CAPACITY: usize = 256;

/// `pair/response`'s payload, returned verbatim by [`CommandRelay::pair`] —
/// decrypting `encrypted_private_key` and persisting the result is the
/// caller's job (`KeyService`), not the relay's.
pub type PairResponsePayload = PairResponseMsg;

/// Seam between `CommandRelay` and the actual MQTT transport. Production
/// wires [`MqttHandle`]; tests wire a capturing/blocking double so publish
/// ordering, timeouts, and routing can be asserted without a live broker.
pub trait Publisher: Send + Sync {
    fn publish(&self, topic: String, payload: Vec<u8>) -> BoxFuture<'static, anyhow::Result<()>>;

    /// QoS 0 publish (config sync resp/hint: never queued for an offline
    /// Hub, never redelivered). Doubles fall back to [`Self::publish`].
    fn publish_qos0(
        &self,
        topic: String,
        payload: Vec<u8>,
    ) -> BoxFuture<'static, anyhow::Result<()>> {
        self.publish(topic, payload)
    }
}

impl Publisher for MqttHandle {
    fn publish(&self, topic: String, payload: Vec<u8>) -> BoxFuture<'static, anyhow::Result<()>> {
        let handle = self.clone();
        Box::pin(async move { handle.publish(topic, payload).await })
    }

    fn publish_qos0(
        &self,
        topic: String,
        payload: Vec<u8>,
    ) -> BoxFuture<'static, anyhow::Result<()>> {
        let handle = self.clone();
        Box::pin(async move { handle.publish_qos0(topic, payload).await })
    }
}

/// Why `execute` refused before signing. The slug is what §16's UI-MQ2
/// requires on the control that issued the command — never only a log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No registry row. There is no fallback topic, so nothing is
    /// published — v1's `reset_export_cursor` defect (mqtt.md quirk #3).
    UnknownCommand,
    /// The caller's role does not reach the verb's level.
    LevelDenied(fiber_protocol::Level),
    /// The signer's certificate is missing the verb's permission or its
    /// `level:*` string.
    PermissionMissing(String),
}

impl Refusal {
    /// The stable slug the UI keys on (design.md §16 UI-MQ2).
    pub fn slug(&self) -> &'static str {
        match self {
            Refusal::UnknownCommand => "unknown_command",
            Refusal::LevelDenied(_) => "level_denied",
            Refusal::PermissionMissing(_) => "permission_missing",
        }
    }

    /// The [`CommandOutcome::error_code`] for this refusal. An unknown verb
    /// is a portal bug, not something an operator can act on, so it stays
    /// unclassified.
    fn error_code(&self) -> Option<&'static str> {
        match self {
            Refusal::UnknownCommand => None,
            Refusal::LevelDenied(_) | Refusal::PermissionMissing(_) => {
                Some(error_code::NOT_PERMITTED)
            }
        }
    }

    fn message(&self, command_type: &str) -> String {
        match self {
            Refusal::UnknownCommand => format!("unknown_command: {command_type}"),
            Refusal::LevelDenied(level) => {
                format!("level_denied: {command_type} is {level}")
            }
            Refusal::PermissionMissing(permission) => {
                format!("permission_missing: {permission}")
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("publish failed: {0}")]
    Publish(#[from] anyhow::Error),
    #[error("pairing timed out waiting for pair/response")]
    Timeout,
    #[error("relay shut down before a pairing response arrived")]
    Closed,
}

/// The failure classes a [`CommandOutcome::error_code`] can name. Each is a
/// `monitoring.command_error.<slug>` key in both catalogues; only classes
/// some path actually produces are listed.
pub mod error_code {
    /// The Hub's in-process radio seam has no rendered config (or its
    /// radio is off / in the other mode): `crate::radio::RadioError`.
    pub const RADIO_NOT_CONFIGURED: &str = "radio_not_configured";
    /// ChirpStack (or the radio daemon) did not answer the seam.
    pub const RADIO_UNAVAILABLE: &str = "radio_unavailable";
    /// The radio network does not know the Node's DevEUI.
    pub const NODE_UNKNOWN: &str = "node_unknown";
    /// The device did not answer within `COMMAND_TIMEOUT`.
    pub const NO_ANSWER: &str = "no_answer";
    /// The frame could not be put on MQTT: the broker is unreachable.
    pub const MQTT_UNAVAILABLE: &str = "mqtt_unavailable";
    /// The portal refused to sign: the caller's role or certificate does
    /// not reach the verb.
    pub const NOT_PERMITTED: &str = "not_permitted";
    /// The device answered with a non-success `config/response`.
    pub const DEVICE_REJECTED: &str = "device_rejected";
    /// Node command results control-core publishes itself, without the
    /// Node refusing anything (control-core `libs/mqtt/monitor.rs`): the
    /// exchange with the Node failed, the verb is in its per-Node cooldown,
    /// or another action still holds the Node's single slot. The slug is
    /// the `result` value on the wire.
    pub const TRANSPORT_ERROR: &str = "transport_error";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const DEVICE_BUSY: &str = "device_busy";
    /// The Hub refused a P2P registration or move: another Node on P2P already holds the
    /// address, the DevEUI's low 32 bits (control.radio `AddressCollision`, exit 16;
    /// p2p_tower_gateway.md §10.1, #470 H4).
    pub const ADDRESS_COLLISION: &str = "address_collision";
    /// The Hub gave a Node's downlink up after its unacked attempts (control.radio's
    /// `undelivered` command document, p2p_tower_gateway.md §10.5 amended 2026-10-07). The
    /// slug is the `result` the Portal gives that document.
    pub const UNDELIVERED: &str = "undelivered";
    /// An `undelivered` document that names its `reason`: the network server discarded the
    /// frame before sending it (#129, a LoRaWAN item too large for the Node's data rate), so
    /// no attempt was made. Not a wire `result`; the Portal tells it apart by the reason.
    pub const NOT_SENT: &str = "not_sent";

    /// Every slug, for the catalogue-coverage test.
    #[cfg(test)]
    pub const ALL: [&str; 13] = [
        RADIO_NOT_CONFIGURED,
        RADIO_UNAVAILABLE,
        NODE_UNKNOWN,
        NO_ANSWER,
        MQTT_UNAVAILABLE,
        NOT_PERMITTED,
        DEVICE_REJECTED,
        TRANSPORT_ERROR,
        RATE_LIMITED,
        DEVICE_BUSY,
        ADDRESS_COLLISION,
        UNDELIVERED,
        NOT_SENT,
    ];
}

/// The class of a Hub's refusal from its `config/response` text. control-core answers a
/// radio verb with control.radio's own error line, `<Kind> [<transport>/<component>]: …`
/// (`radio_nodes::run`), so its `Kind` is the line's first word; the `Kind` names are
/// control.radio's consumed contract. Only a kind with its own sentence is classified, every
/// other refusal stays `device_rejected`.
fn hub_refusal_code(message: Option<&str>) -> &'static str {
    let kind = message
        .and_then(|m| m.split_once(" ["))
        .map(|(kind, _)| kind.trim());
    if kind == Some(control_radio::error::Kind::AddressCollision.as_str()) {
        error_code::ADDRESS_COLLISION
    } else {
        error_code::DEVICE_REJECTED
    }
}

/// Outcome of [`CommandRelay::execute`] — deliberately the same field shape
/// `templates/partials/command_status.html`'s `cmd` context expects (minus
/// `ts`, which the route layer stamps at render time), so a route handler
/// can hand this straight to `render::` without reshaping it.
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    /// `None` only for the "unknown command_type" error path, where nothing
    /// was ever signed or published — there is no request to correlate.
    pub request_id: Option<String>,
    pub command_type: String,
    /// `"success"` | `"error"` | `"timeout"` from [`CommandRelay::execute`],
    /// which waits for the device; `"sent"` | `"error"` from
    /// [`CommandRelay::dispatch`], which does not.
    ///
    /// `"sent"` is **not** the flat dialect's old terminal status coming
    /// back — that routing is gone. It is the answer to a different
    /// question, and only `dispatch` asks it: the frame is on the wire, and
    /// whatever the device says about it will arrive on the status
    /// broadcast rather than in this value.
    pub status: &'static str,
    pub message: Option<String>,
    /// The failure class the UI can explain in the operator's language —
    /// `partials/command_status.html` renders `monitoring.command_error.
    /// <slug>` as the sentence and keeps `message` as the technical detail.
    /// `None` on success and on a failure no path has classified, which
    /// renders exactly as it did before slugs existed.
    pub error_code: Option<&'static str>,
    /// The device's structured answer, `config/response.result`, for the few
    /// verbs that have one — today `cluster-client-add` / `-remove`
    /// (plan/control/cluster.md §7). Present on an `"error"` too when the
    /// verb did part of its work: a minted account whose gateway failed still
    /// carries its password.
    ///
    /// It reaches the caller of [`CommandRelay::execute`] and nothing else:
    /// [`CommandStatus`] has no such field, so no SSE frame carries it,
    /// `render::CommandOutcomeCtx` drops it, so no fragment renders it, and
    /// its `Debug` prints keys only, so no `{:?}` of an outcome logs it.
    pub result: Option<ResponseResult>,
}

/// One transition in a signed command's lifecycle, broadcast for the SSE
/// hub to render as `templates/partials/command_status.html`. Field names
/// match that template's `cmd` context exactly (`ts` still needs
/// `render.rs`'s `format_time`-style formatting at the SSE layer, so it
/// rides as a raw unix timestamp here).
#[derive(Debug, Clone)]
pub struct CommandStatus {
    pub request_id: String,
    pub command_type: String,
    /// `"sent"` | `"challenge"` | `"confirmed"` | `"success"` | `"error"` |
    /// `"timeout"`.
    pub status: &'static str,
    pub message: Option<String>,
    /// Same meaning as [`CommandOutcome::error_code`], so the SSE frame and
    /// the POST response explain one failure with the same sentence.
    pub error_code: Option<&'static str>,
    /// The device the command is about (GitLab #121): a Beacon's MAC, a Node's DevEUI or
    /// the Hub's hostname, as [`command_target`] read it off the request. `None` for a
    /// request this relay did not publish. The SSE layer resolves it to the device's name.
    pub target: Option<String>,
    pub ts: i64,
}

/// How long a request's target is kept for its later status frames: past the 4 h a
/// pulled save may wait for the Hub's ack (decision #43), so the "delivered" frame still
/// names the device.
const TARGET_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// The device a command is about (GitLab #121): the Beacon (`mac`) or Node (`dev_eui`,
/// `node_key`) its params name, else the Hub it is sent to.
pub fn command_target(hostname: &str, params: &Value) -> String {
    ["mac", "dev_eui", "node_key"]
        .iter()
        .find_map(|k| params.get(*k).and_then(Value::as_str))
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(hostname)
        .to_string()
}

/// `(hub lowercase, item key)` of a save the Hub pulls.
type SyncSaveKey = (String, String);
/// `(request_id, command_type, gen)` of that save.
type SyncSave = (String, String, u64);

/// A signed `config_request` awaiting its challenge/response — everything
/// [`CommandRelay::run`] needs to build and sign the matching
/// `config_confirm`, plus the oneshot that wakes the waiting `execute` call.
/// The device hostname isn't stored here: `handle_challenge` gets it
/// straight off the `config/challenge` envelope itself (which topic it
/// arrived on), and that's the trustworthy value — not whatever `execute`
/// was told to publish to.
struct PendingCommand {
    command_type: String,
    signer_id: String,
    seed: Zeroizing<[u8; 32]>,
    certificate: Option<Certificate>,
    tx: oneshot::Sender<CommandOutcome>,
}

/// The signed-command relay: owns the publish handle, the in-flight
/// request/challenge/pairing correlation maps, and the status broadcast the
/// SSE hub renders from. Cheap to clone (every field is `Arc`-backed or
/// itself a broadcast sender) — the same pattern `AuthService`/`KeyService`
/// use for cloning into `AppState` per request.
#[derive(Clone)]
pub struct CommandRelay {
    publisher: Arc<dyn Publisher>,
    topic_prefix: Arc<str>,
    /// `request_id` -> the command awaiting its challenge/response.
    pending: Arc<Mutex<HashMap<String, PendingCommand>>>,
    /// `challenge_id` -> (`request_id`, when it was first seen), populated
    /// once a challenge for a known pending command arrives (mqtt.md:
    /// `config/response` only carries `challenge_id` reliably; `request_id`
    /// is best-effort).
    ///
    /// The timestamp makes this the seen-set of design.md §6.2 as well as
    /// the correlation map: **rumqttc replays unacked publishes on
    /// reconnect regardless of `clean_session`**, so a challenge can arrive
    /// twice, and answering it twice would burn a second nonce and produce
    /// a `ChallengeNotFound` the operator would see as a failure. One map
    /// serves both jobs; entries older than [`CHALLENGE_TTL`] are swept on
    /// insert.
    challenges: Arc<Mutex<HashMap<String, (String, Instant)>>>,
    /// Every nonce this relay has signed, `nonce` -> (`request_id`, when
    /// it was signed), for the duplicate-error tolerance of design.md
    /// §6.2.
    ///
    /// The `errors` topic carries no correlation key — `DeviceErrorMsg` is
    /// `{timestamp, command, error, message}` and the device publishes
    /// `publish_error("config_request", "authorization_failed", "Nonce has
    /// already been used (replay attack): <nonce>")`. The nonce inside that
    /// prose is the only token shared with the frame we sent, so it is the
    /// key. It is a *better* key than `request_id` would be for this one
    /// job: the thing being recognised is "this exact signed frame arrived
    /// twice", and the nonce is what makes a frame exact.
    ///
    /// Entries older than [`CHALLENGE_TTL`] are swept on insert, matching
    /// the seen-set above.
    signed_nonces: Arc<Mutex<HashMap<String, (String, Instant)>>>,
    /// `request_id` -> the waiter for a `pair/response`.
    pairing: Arc<Mutex<HashMap<String, oneshot::Sender<PairResponsePayload>>>>,
    status_tx: broadcast::Sender<CommandStatus>,
    /// Config sync cut-over (config_sync.md amendment 2026-10-15 B): a save
    /// the Hub pulls rather than receives, `(hub lowercase, item key)` ->
    /// `(request_id, command_type, gen)`. The Hub's ack of that gen settles
    /// it on the status broadcast as "delivered" or "stored".
    sync_saves: Arc<Mutex<HashMap<SyncSaveKey, SyncSave>>>,
    /// `request_id` -> (the device it is about, when it was noted), so every status frame
    /// of that request names the device (GitLab #121). Swept on insert after [`TARGET_TTL`].
    targets: Arc<Mutex<HashMap<String, (String, Instant)>>>,
    /// Every `(topic, body)` this relay published, for tests that need to
    /// assert what did — or did not — reach the wire.
    ///
    /// A route test's most important assertion is usually a negative one
    /// ("a draft write publishes nothing", "an invalid registration
    /// publishes nothing"), and the alternative to recording here is a
    /// bespoke publisher per test plus a way to reach it through
    /// `AppState` — the same tape, threaded by hand.
    #[cfg(test)]
    published: Arc<Mutex<Vec<(String, Value)>>>,
}

impl CommandRelay {
    pub fn new(publisher: impl Publisher + 'static, topic_prefix: impl Into<String>) -> Self {
        let (status_tx, _rx) = broadcast::channel(STATUS_CHANNEL_CAPACITY);
        Self {
            publisher: Arc::new(publisher),
            topic_prefix: Arc::from(topic_prefix.into()),
            pending: Arc::new(Mutex::new(HashMap::new())),
            challenges: Arc::new(Mutex::new(HashMap::new())),
            signed_nonces: Arc::new(Mutex::new(HashMap::new())),
            pairing: Arc::new(Mutex::new(HashMap::new())),
            status_tx,
            sync_saves: Arc::new(Mutex::new(HashMap::new())),
            targets: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            published: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Remembers which device `request_id` is about, for its status frames and for
    /// [`Self::target_of`] (GitLab #121).
    pub(crate) fn note_target(&self, request_id: &str, target: &str) {
        let mut targets = self.targets.lock();
        let now = Instant::now();
        targets.retain(|_, (_, at)| now.duration_since(*at) < TARGET_TTL);
        targets.insert(request_id.to_string(), (target.to_string(), now));
    }

    /// The device `request_id` is about, if this relay noted one.
    pub(crate) fn target_of(&self, request_id: &str) -> Option<String> {
        self.targets.lock().get(request_id).map(|(t, _)| t.clone())
    }

    /// Remembers a save the Hub will pull: its ack settles `request_id`.
    /// A newer save of the same key replaces it (the older one is moot).
    pub(crate) fn track_sync_save(
        &self,
        hub: &str,
        key: &str,
        request_id: &str,
        command_type: &str,
        gen: u64,
    ) {
        self.sync_saves.lock().insert(
            (hub.to_ascii_lowercase(), key.to_string()),
            (request_id.to_string(), command_type.to_string(), gen),
        );
    }

    /// The Hub acked `gen`: every tracked save of this Hub at or below it is
    /// settled, "delivered" unless `failed` names its key (then "stored",
    /// the device copy not delivered, with the Hub's reason).
    pub(crate) fn settle_sync_saves(&self, hub: &str, gen: u64, failed: &HashMap<String, String>) {
        let hub = hub.to_ascii_lowercase();
        let settled: Vec<(SyncSaveKey, SyncSave)> = {
            let mut saves = self.sync_saves.lock();
            let keys: Vec<(String, String)> = saves
                .iter()
                .filter(|((h, _), (_, _, g))| *h == hub && *g <= gen)
                .map(|(k, _)| k.clone())
                .collect();
            keys.into_iter()
                .filter_map(|k| saves.remove(&k).map(|v| (k, v)))
                .collect()
        };
        for ((_, key), (request_id, command_type, _)) in settled {
            match failed.get(&key) {
                None => self.emit_status(&request_id, &command_type, "delivered", None, None),
                Some(error) => self.emit_status(
                    &request_id,
                    &command_type,
                    "stored",
                    Some(error.clone()),
                    Some("device_copy_not_delivered"),
                ),
            }
        }
    }

    /// Publishes, recording the frame under `cfg(test)`.
    ///
    /// Every publish in this module goes through here, so a test cannot be
    /// fooled by a fifth call site added later.
    async fn publish(&self, topic: String, body: Vec<u8>) -> anyhow::Result<()> {
        #[cfg(test)]
        self.published.lock().push((
            topic.clone(),
            serde_json::from_slice(&body).unwrap_or(Value::Null),
        ));
        self.publisher.publish(topic, body).await
    }

    /// An unsigned publish outside the command handshake — the config-sync
    /// response and hint (plan/portal/config_sync.md §5). Goes through
    /// [`Self::publish`], so the test tape sees it too.
    /// QoS 0 (plan/portal/config_sync.md §5, amendment 2026-10-15).
    pub(crate) async fn publish_raw(&self, topic: String, body: Vec<u8>) -> anyhow::Result<()> {
        #[cfg(test)]
        self.published.lock().push((
            topic.clone(),
            serde_json::from_slice(&body).unwrap_or(Value::Null),
        ));
        self.publisher.publish_qos0(topic, body).await
    }

    /// The configured topic prefix (`proximos`).
    pub(crate) fn topic_prefix(&self) -> &str {
        &self.topic_prefix
    }

    /// The frames this relay has published, newest last. Test-only.
    #[cfg(test)]
    pub(crate) fn published(&self) -> Vec<(String, Value)> {
        self.published.lock().clone()
    }

    /// Constructs a relay wired to a real MQTT broker and returns, along
    /// with it, the `IngestEvent` sender to pass as `run_ingest`'s
    /// `relay_tx` third sink. The returned relay's [`CommandRelay::run`]
    /// task is already spawned.
    pub fn spawn(
        mqtt: MqttHandle,
        topic_prefix: impl Into<String>,
    ) -> (Self, mpsc::UnboundedSender<IngestEvent>) {
        let relay = Self::new(mqtt, topic_prefix);
        let (tx, rx) = mpsc::unbounded_channel();
        let task_relay = relay.clone();
        tokio::spawn(async move { task_relay.run(rx).await });
        (relay, tx)
    }

    pub fn subscribe_status(&self) -> broadcast::Receiver<CommandStatus> {
        self.status_tx.subscribe()
    }

    fn emit_status(
        &self,
        request_id: &str,
        command_type: &str,
        status: &'static str,
        message: Option<String>,
        error_code: Option<&'static str>,
    ) {
        // No subscribers is normal (no browser tab open) — a send error
        // just means nobody was listening, same reasoning as StateStore.
        let _ = self.status_tx.send(CommandStatus {
            request_id: request_id.to_string(),
            command_type: command_type.to_string(),
            status,
            message,
            error_code,
            target: self.target_of(request_id),
            ts: now_unix(),
        });
    }

    /// Signs, stores, publishes and awaits the full challenge/confirm/
    /// response handshake. There is one dialect and one topic now
    /// (design.md §7), so the registry lookup exists to decide whether the
    /// verb is publishable at all and — from P2 — to gate it; an unknown
    /// `command_type` errors without publishing anywhere, no
    /// dead-fallback-topic publish like v1's `reset_export_cursor` defect
    /// (mqtt.md quirk #3).
    ///
    /// This is the entry point for anything an operator is waiting on:
    /// the returned status is the device's own answer, which is what makes
    /// a refusal surfaceable on the control that issued it (§16 UI-MQ2).
    /// [`Self::dispatch`] is the one for work nobody is waiting on.
    pub async fn execute(
        &self,
        user_label: &str,
        key: &UserSigningKey,
        hostname: &str,
        command_type: &str,
        params: Value,
        reason: Option<String>,
    ) -> CommandOutcome {
        tracing::debug!(
            user_label,
            hostname,
            command_type,
            "relay: executing command"
        );

        if let Some(refusal) = Self::refuse_before_signing(user_label, key, hostname, command_type)
        {
            return refusal;
        }

        self.execute_config_request(key, hostname, command_type, params, reason)
            .await
    }

    /// Signs and publishes, and returns as soon as the frame is on the wire
    /// — status `"sent"`, without waiting out [`COMMAND_TIMEOUT`] for the
    /// device.
    ///
    /// For the callers that have never had anyone waiting on the device's
    /// answer and do not need one: the alarm ack's `silence_buzzer` side
    /// effect and backfill's `get_sticker_history` downlink. Both were
    /// fire-and-forget publishes before the rebuild, and making them wait
    /// put a 20 s stall inside an HTTP handler —
    /// `POST /alarms/bulk-acknowledge` pays it once per checked alarm, so a
    /// screenful of alarms against an offline Hub outlives the proxy — and
    /// once per node inside `backfill`'s drain pass.
    ///
    /// **Nothing is skipped.** Same `authorize` gate, same v2 envelope, same
    /// nonce ledger, same pending entry. Only the wait moves, onto a spawned
    /// task, so the entry is still swept on timeout and the device's real
    /// answer still reaches `command_status` subscribers — the SSE hub sees
    /// the identical `sent` → `challenge` → … → `success` sequence either
    /// way. A refusal before signing is still returned **here**,
    /// synchronously, because it happens before anything is published: a
    /// caller can still tell "the portal would not send this" from "the
    /// portal sent it".
    pub async fn dispatch(
        &self,
        user_label: &str,
        key: &UserSigningKey,
        hostname: &str,
        command_type: &str,
        params: Value,
        reason: Option<String>,
    ) -> CommandOutcome {
        tracing::debug!(
            user_label,
            hostname,
            command_type,
            "relay: dispatching command"
        );

        if let Some(refusal) = Self::refuse_before_signing(user_label, key, hostname, command_type)
        {
            return refusal;
        }

        match self
            .publish_config_request(key, hostname, command_type, params, reason)
            .await
        {
            Ok((request_id, rx)) => {
                let relay = self.clone();
                let hostname = hostname.to_string();
                let command_type = command_type.to_string();
                let outcome = CommandOutcome {
                    request_id: Some(request_id.clone()),
                    command_type: command_type.clone(),
                    status: "sent",
                    message: None,
                    error_code: None,
                    result: None,
                };
                tokio::spawn(async move {
                    // Nobody is holding the result, so this log line is the
                    // only place a detached failure surfaces until UI-MQ2's
                    // device-rejection half lands (design.md §13 item 9).
                    let settled = relay.await_outcome(request_id, command_type, rx).await;
                    if settled.status != "success" {
                        tracing::warn!(
                            hostname,
                            command_type = %settled.command_type,
                            status = settled.status,
                            message = ?settled.message,
                            "relay: dispatched command did not succeed"
                        );
                    }
                });
                outcome
            }
            Err(outcome) => outcome,
        }
    }

    /// §4.4's gate applied: `Some` is the terminal outcome both
    /// [`Self::execute`] and [`Self::dispatch`] return for a command this
    /// portal will not sign, with the one log line that records why.
    fn refuse_before_signing(
        user_label: &str,
        key: &UserSigningKey,
        hostname: &str,
        command_type: &str,
    ) -> Option<CommandOutcome> {
        let Err(slug) = Self::authorize(key, command_type) else {
            return None;
        };
        tracing::info!(
            user_label,
            hostname,
            command_type,
            slug = slug.slug(),
            "relay: refused before signing"
        );
        Some(CommandOutcome {
            request_id: None,
            command_type: command_type.to_string(),
            status: "error",
            message: Some(slug.message(command_type)),
            error_code: slug.error_code(),
            result: None,
        })
    }

    /// §4.4's refuse-before-sign gate — the one place a command is stopped
    /// on the portal side, run before anything is signed or published.
    ///
    /// Three checks, in the order that produces the most useful slug:
    ///
    /// 1. the verb is in the registry at all (`unknown_command`);
    /// 2. the caller's **live** role reaches the verb's level
    ///    (`level_denied`) — not the level their certificate happens to
    ///    carry, because `ensure_user_key` re-mints with the union and
    ///    never removes a permission, so a demoted user's certificate keeps
    ///    the old `level:*` strings;
    /// 3. the certificate the command will actually ride with carries the
    ///    verb's permission and `level:<level>` (`permission_missing`) —
    ///    the same pair the device checks at step 8, applied early so the
    ///    operator sees a slug instead of a device rejection arriving on
    ///    the `errors` topic. This is why the preflight is universal now
    ///    rather than v1's hand-picked quartet.
    ///
    /// The fourth of §4.4's steps — the level is written into the envelope
    /// from the registry, never from the caller — is structural: it lives
    /// in `fiber_protocol::build_request`, which takes no level argument.
    fn authorize(key: &UserSigningKey, command_type: &str) -> Result<(), Refusal> {
        let Some(spec) = fiber_protocol::lookup(command_type) else {
            return Err(Refusal::UnknownCommand);
        };

        if spec.level > key.ceiling {
            return Err(Refusal::LevelDenied(spec.level));
        }

        // The certificate rides on the wire, so this reads the one that
        // will actually be sent rather than re-deriving what it should be.
        let carries = |permission: &str| {
            key.certificate
                .get("permissions")
                .and_then(Value::as_array)
                .is_some_and(|list| list.iter().any(|p| p.as_str() == Some(permission)))
        };
        if !carries(spec.permission) {
            return Err(Refusal::PermissionMissing(spec.permission.to_string()));
        }
        let level_permission = spec.level.permission();
        if !carries(&level_permission) {
            return Err(Refusal::PermissionMissing(level_permission));
        }
        Ok(())
    }

    /// Publish, then wait for the device. The signing half is
    /// [`Self::publish_config_request`] and the waiting half
    /// [`Self::await_outcome`]; [`Self::dispatch`] is the same pair with the
    /// second one detached.
    async fn execute_config_request(
        &self,
        key: &UserSigningKey,
        hostname: &str,
        command_type: &str,
        params: Value,
        reason: Option<String>,
    ) -> CommandOutcome {
        match self
            .publish_config_request(key, hostname, command_type, params, reason)
            .await
        {
            Ok((request_id, rx)) => {
                // The wait runs on its own task, not inline in the caller's
                // future. axum drops a handler future the moment the HTTP
                // client goes away, and a dropped `await_outcome` never
                // reaches its timeout arm — so the `pending` entry, signing
                // seed and certificate included, would outlive the request
                // for the life of the process, one per abandoned command.
                // Spawned, the sweep happens whether or not anyone is still
                // listening; awaiting the handle is what turns the outcome
                // back into a return value for the caller that stayed
                // (`a_dropped_execute_still_sweeps_its_pending_entry`).
                let relay = self.clone();
                let waiter_request_id = request_id.clone();
                let waiter_command_type = command_type.to_string();
                let waiter = tokio::spawn(async move {
                    relay
                        .await_outcome(waiter_request_id, waiter_command_type, rx)
                        .await
                });
                match waiter.await {
                    Ok(outcome) => outcome,
                    Err(join) => CommandOutcome {
                        request_id: Some(request_id),
                        command_type: command_type.to_string(),
                        status: "error",
                        message: Some(format!("relay waiter failed: {join}")),
                        error_code: None,
                        result: None,
                    },
                }
            }
            Err(outcome) => outcome,
        }
    }

    /// Everything up to and including the publish: parse the stored
    /// certificate, sign the v2 envelope, remember the nonce, store the
    /// pending entry, put the frame on the wire.
    ///
    /// `Ok` means the frame left the portal — `"sent"` has already gone out
    /// on the status broadcast — and hands back the `request_id` with the
    /// receiver the device's answer will arrive on. `Err` is the terminal
    /// [`CommandOutcome`] for a command that never got that far, and on
    /// every one of those paths `pending` is left exactly as it was found.
    async fn publish_config_request(
        &self,
        key: &UserSigningKey,
        hostname: &str,
        command_type: &str,
        params: Value,
        reason: Option<String>,
    ) -> Result<(String, oneshot::Receiver<CommandOutcome>), CommandOutcome> {
        let target = command_target(hostname, &params);
        let certificate = match serde_json::from_value::<Certificate>(key.certificate.clone()) {
            Ok(cert) => Some(cert),
            Err(err) => {
                return Err(CommandOutcome {
                    request_id: None,
                    command_type: command_type.to_string(),
                    status: "error",
                    message: Some(format!("invalid stored certificate: {err}")),
                    error_code: None,
                    result: None,
                });
            }
        };

        let Some(signed) = fiber_protocol::build_request(
            command_type,
            params,
            reason.as_deref(),
            &key.signer_id,
            &key.seed,
            certificate.as_ref(),
        ) else {
            // `execute`/`dispatch` already resolved `command_type` through
            // the same registry, so this is unreachable — handled rather
            // than unwrapped so a future caller that skips that check
            // cannot turn it into a panic.
            return Err(CommandOutcome {
                request_id: None,
                command_type: command_type.to_string(),
                status: "error",
                message: Some(format!("unknown command_type: {command_type}")),
                error_code: None,
                result: None,
            });
        };
        let request_id = signed.request_id.clone();
        self.remember_nonce(&signed.nonce, &request_id);
        self.note_target(&request_id, &target);

        let (tx, rx) = oneshot::channel();
        // STORE PENDING FIRST — mqtt.md's ordering guarantee (module doc):
        // the device's challenge can arrive before an after-publish write
        // would land. This insert is synchronous and happens before the
        // `.await` below, unconditionally.
        self.pending.lock().insert(
            request_id.clone(),
            PendingCommand {
                command_type: command_type.to_string(),
                signer_id: key.signer_id.clone(),
                seed: key.seed.clone(),
                certificate,
                tx,
            },
        );

        let body = match serde_json::to_vec(&signed) {
            Ok(body) => body,
            Err(err) => {
                self.pending.lock().remove(&request_id);
                return Err(CommandOutcome {
                    request_id: Some(request_id),
                    command_type: command_type.to_string(),
                    status: "error",
                    message: Some(err.to_string()),
                    error_code: None,
                    result: None,
                });
            }
        };
        // Decision #21: the agent listens under the lowercase spelling.
        let hostname = hostname.to_ascii_lowercase();
        let topic = format!("{}/{hostname}/commands/config/request", self.topic_prefix);

        if let Err(err) = self.publish(topic, body).await {
            self.pending.lock().remove(&request_id);
            self.emit_status(
                &request_id,
                command_type,
                "error",
                Some(err.to_string()),
                Some(error_code::MQTT_UNAVAILABLE),
            );
            return Err(CommandOutcome {
                request_id: Some(request_id),
                command_type: command_type.to_string(),
                status: "error",
                message: Some(err.to_string()),
                error_code: Some(error_code::MQTT_UNAVAILABLE),
                result: None,
            });
        }

        self.emit_status(&request_id, command_type, "sent", None, None);
        Ok((request_id, rx))
    }

    /// The device's half: wait out [`COMMAND_TIMEOUT`] for the answer
    /// `handle_response` routes into `rx`, and turn whatever happens into
    /// the terminal [`CommandOutcome`].
    ///
    /// The timeout's `pending` removal lives **here** rather than at the
    /// call site precisely because [`Self::dispatch`] runs this on a
    /// spawned task: a command nobody is waiting on still has to sweep its
    /// own entry, or the map grows by one per unanswered command for the
    /// lifetime of the process.
    async fn await_outcome(
        &self,
        request_id: String,
        command_type: String,
        rx: oneshot::Receiver<CommandOutcome>,
    ) -> CommandOutcome {
        match tokio::time::timeout(COMMAND_TIMEOUT, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => CommandOutcome {
                request_id: Some(request_id),
                command_type,
                status: "error",
                message: Some("relay shut down before a response arrived".to_string()),
                error_code: None,
                result: None,
            },
            Err(_) => {
                self.pending.lock().remove(&request_id);
                self.emit_status(
                    &request_id,
                    &command_type,
                    "timeout",
                    None,
                    Some(error_code::NO_ANSWER),
                );
                CommandOutcome {
                    request_id: Some(request_id),
                    command_type,
                    status: "timeout",
                    message: None,
                    error_code: Some(error_code::NO_ANSWER),
                    result: None,
                }
            }
        }
    }

    /// Sends `pair/request` and awaits the matching `pair/response`,
    /// 30s timeout (mqtt.md §"Pairing"). Decrypting `encrypted_private_key`
    /// and persisting the result through `KeyService` is the caller's job.
    ///
    /// `local` tells the device this portal runs on it, so it should encrypt
    /// the key under `fiber_protocol::LOCAL_PAIRING_PASSWORD` instead of the
    /// code on its LCD — the caller must decrypt with the same choice. It is
    /// always sent, never omitted: an older device ignores the unknown field
    /// and answers under the displayed code, which then fails to decrypt
    /// rather than pairing under a password the operator never chose.
    pub async fn pair(
        &self,
        hostname: &str,
        admin_username: &str,
        local: bool,
    ) -> Result<PairResponsePayload, RelayError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pairing.lock().insert(request_id.clone(), tx);

        let body = serde_json::json!({
            "request_id": request_id,
            "timestamp": now_unix(),
            "admin_username": admin_username,
            "local": local,
        });
        let bytes = serde_json::to_vec(&body).map_err(|err| RelayError::Publish(err.into()))?;
        // Decision #21: the agent listens under the lowercase spelling.
        let hostname = hostname.to_ascii_lowercase();
        let topic = format!("{}/{hostname}/pair/request", self.topic_prefix);

        if let Err(err) = self.publish(topic, bytes).await {
            self.pairing.lock().remove(&request_id);
            return Err(RelayError::Publish(err));
        }

        match tokio::time::timeout(PAIR_TIMEOUT, rx).await {
            Ok(Ok(payload)) => Ok(payload),
            Ok(Err(_)) => Err(RelayError::Closed),
            Err(_) => {
                self.pairing.lock().remove(&request_id);
                Err(RelayError::Timeout)
            }
        }
    }

    /// Drives the relay's ingest sink until the sender side (the MQTT
    /// dispatch loop) closes. Only `ConfigChallenge`/`ConfigResponse`/
    /// `PairResponse`/`DeviceError` ever arrive here (`mqtt::dispatch`
    /// filters the channel to exactly those four) — the wildcard arm exists
    /// purely as a forward-compatible guard, not because other variants are
    /// expected.
    pub async fn run(&self, mut rx: mpsc::UnboundedReceiver<IngestEvent>) {
        while let Some(event) = rx.recv().await {
            match event {
                IngestEvent::ConfigChallenge { hostname, payload } => {
                    self.handle_challenge(&hostname, payload).await;
                }
                IngestEvent::ConfigResponse { payload, .. } => {
                    self.handle_response(payload);
                }
                IngestEvent::PairResponse { payload, .. } => {
                    self.handle_pair_response(payload);
                }
                IngestEvent::DeviceError { hostname, payload } => {
                    self.handle_device_error(&hostname, &payload);
                }
                other => {
                    tracing::debug!(?other, "CommandRelay::run: unexpected event, ignoring");
                }
            }
        }
    }

    /// Records a nonce this relay just signed, sweeping the ledger first.
    ///
    /// Bounded by [`CHALLENGE_TTL`] rather than by count: the device's own
    /// replay window is what makes an old nonce uninteresting, and a portal
    /// signing fast enough to make this map large is already publishing
    /// faster than the device's 10/s rate limiter accepts.
    fn remember_nonce(&self, nonce: &str, request_id: &str) {
        let mut ledger = self.signed_nonces.lock();
        let now = Instant::now();
        ledger.retain(|_, (_, signed_at)| now.duration_since(*signed_at) < CHALLENGE_TTL);
        ledger.insert(nonce.to_string(), (request_id.to_string(), now));
    }

    /// Duplicate-error tolerance (design.md §6.2).
    ///
    /// **rumqttc replays unacked publishes on reconnect regardless of
    /// `clean_session`**, so a `config_request` in flight across a
    /// disconnect is published twice. The device verifies the first copy,
    /// records its nonce, and refuses the second with `NonceAlreadyUsed` on
    /// the `errors` topic. That refusal is correct behaviour on both sides
    /// and describes a command that in fact succeeded — surfacing it as a
    /// failure would make every reconnect look like a fault.
    ///
    /// So: an `authorization_failed` on the command dialect whose message
    /// names a nonce **this relay signed** is a debug line and nothing
    /// else. Every other device error keeps today's behaviour — the store's
    /// liveness bump and no [`CommandStatus`] — because the `errors` topic
    /// carries no `request_id`, and correlating on hostname alone would
    /// misattribute the moment two commands are in flight to one Hub. That
    /// missing key is design.md §13 item 9; UI-MQ2's device-rejection half
    /// waits on it.
    fn handle_device_error(&self, hostname: &str, err: &DeviceErrorMsg) {
        let Some(nonce) = err.message.as_deref().and_then(replayed_nonce) else {
            tracing::debug!(
                hostname,
                command = err.command.as_deref().unwrap_or("unknown"),
                error = err.error.as_deref().unwrap_or("unknown"),
                "device error is not a nonce rejection — leaving it to the store"
            );
            return;
        };

        match self.signed_nonces.lock().get(nonce) {
            Some((request_id, _)) => tracing::debug!(
                hostname,
                request_id = %request_id,
                "duplicate-nonce device error for a nonce we signed — rumqttc's replay, ignoring"
            ),
            None => tracing::warn!(
                hostname,
                "nonce rejection for a nonce this relay did not sign — not our replay"
            ),
        }
    }

    async fn handle_challenge(&self, hostname: &str, challenge: ConfigChallengeMsg) {
        let Some(request_id) = challenge.request_id.clone() else {
            tracing::warn!(
                challenge_id = %challenge.challenge_id,
                "config/challenge missing request_id — cannot correlate, dropping"
            );
            return;
        };

        let pending = {
            let map = self.pending.lock();
            map.get(&request_id).map(|p| {
                (
                    p.command_type.clone(),
                    p.signer_id.clone(),
                    p.seed.clone(),
                    p.certificate.clone(),
                )
            })
        };

        // Phase 1 has no interactive fallback (module doc): an unmatched
        // challenge — restart lost it, or it's simply not ours — is logged
        // and dropped rather than routed anywhere else.
        let Some((command_type, signer_id, seed, certificate)) = pending else {
            tracing::warn!(
                challenge_id = %challenge.challenge_id,
                request_id,
                "config/challenge for unknown request_id — no interactive fallback in phase 1, dropping"
            );
            return;
        };

        // The seen-set (design.md §6.2). A replayed challenge is a debug
        // line, never a second confirm: the device removes the challenge
        // when it answers the first one, so a second confirm would be
        // rejected with `ChallengeNotFound` and surface to the operator as
        // a failed command that in fact succeeded.
        {
            let mut challenges = self.challenges.lock();
            let now = Instant::now();
            challenges.retain(|_, (_, seen)| now.duration_since(*seen) < CHALLENGE_TTL);
            if let Some((known, _)) = challenges.get(&challenge.challenge_id) {
                tracing::debug!(
                    challenge_id = %challenge.challenge_id,
                    request_id = %known,
                    "duplicate config/challenge — already confirmed, ignoring"
                );
                return;
            }
            challenges.insert(challenge.challenge_id.clone(), (request_id.clone(), now));
        }
        self.emit_status(&request_id, &command_type, "challenge", None, None);

        let Some(confirm) = fiber_protocol::build_confirm(
            &challenge.challenge_id,
            "APPROVED",
            &signer_id,
            &seed,
            command_type.as_str(),
            None,
            None,
            certificate.as_ref(),
        ) else {
            // Same unreachable-by-construction case as `execute`'s: the
            // pending entry only exists because the verb resolved.
            tracing::warn!(
                request_id,
                command_type,
                "config/challenge for a verb the registry no longer knows — dropping"
            );
            return;
        };

        self.remember_nonce(&confirm.nonce, &request_id);

        let body = match serde_json::to_vec(&confirm) {
            Ok(body) => body,
            Err(err) => {
                tracing::warn!(%err, request_id, "failed to serialize config_confirm");
                return;
            }
        };
        // Decision #21: lowercase here too, for a caller that did not come through routing.
        let hostname = hostname.to_ascii_lowercase();
        let topic = format!("{}/{hostname}/commands/config/confirm", self.topic_prefix);

        if let Err(err) = self.publish(topic, body).await {
            tracing::warn!(%err, request_id, "failed to publish config_confirm");
            self.emit_status(
                &request_id,
                &command_type,
                "error",
                Some(err.to_string()),
                Some(error_code::MQTT_UNAVAILABLE),
            );
            return;
        }

        self.emit_status(&request_id, &command_type, "confirmed", None, None);
    }

    fn handle_response(&self, response: ConfigResponseMsg) {
        // A confirmed command resolves through the challenge map. A
        // single-shot `read`/`operate` never had a challenge (design.md
        // §5.2), so it falls back to the `request_id` the device echoes —
        // which is the same field the challenge path uses to correlate.
        //
        // The entry is *not* removed here: it is the seen-set, and a
        // replayed challenge can still arrive after the response. It ages
        // out on the next insert instead.
        let request_id = {
            let challenges = self.challenges.lock();
            challenges
                .get(&response.challenge_id)
                .map(|(request_id, _)| request_id.clone())
                .or_else(|| response.request_id.clone())
        };

        let Some(request_id) = request_id else {
            tracing::warn!(
                challenge_id = %response.challenge_id,
                "config/response unresolvable — no challenge mapping and no request_id"
            );
            return;
        };

        let Some(pending) = self.pending.lock().remove(&request_id) else {
            // Not a fault: rumqttc replays unacked publishes on reconnect
            // regardless of `clean_session`, so a device response can
            // arrive twice for a request this relay already resolved
            // (design.md §6.1, §6.2). A debug line, not a warning.
            tracing::debug!(
                request_id,
                "config/response for an already-resolved request — replay, ignoring"
            );
            return;
        };

        let (status, error_code) = if response.status.eq_ignore_ascii_case("success") {
            ("success", None)
        } else {
            ("error", Some(hub_refusal_code(response.message.as_deref())))
        };
        self.emit_status(
            &request_id,
            &pending.command_type,
            status,
            response.message.clone(),
            error_code,
        );

        // `result` goes to the waiter alone — `emit_status` above took the
        // message and nothing else (`CommandOutcome::result`).
        let outcome = CommandOutcome {
            request_id: Some(request_id),
            command_type: pending.command_type,
            status,
            message: response.message,
            error_code,
            result: response.result,
        };
        // The waiter may already be gone (e.g. it timed out first) — a
        // failed send here is harmless.
        let _ = pending.tx.send(outcome);
    }

    fn handle_pair_response(&self, payload: PairResponsePayload) {
        let Some(waiter) = self.pairing.lock().remove(&payload.request_id) else {
            tracing::debug!(request_id = %payload.request_id, "pair/response for unknown/expired waiter");
            return;
        };
        let _ = waiter.send(payload);
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The nonce out of a device `errors` payload, when that payload is the
/// replay rejection.
///
/// The device renders `CryptoError` with `{:?}` inside
/// `AuthError::SignatureVerificationFailed`, which its own `Display` then
/// wraps, so the wire text today is
/// `Signature verification failed: NonceAlreadyUsed("<32 hex>")`. Matching
/// the variant name **or** the `Display` wording keeps this working if the
/// device ever swaps one rendering for the other, and the nonce itself is
/// lifted by shape — a 32-character lowercase-hex run, which is exactly
/// what `generate_nonce` produces — rather than by position in the prose.
///
/// Being wrong here is cheap by construction: a mis-lifted token simply
/// misses the ledger in [`CommandRelay::handle_device_error`] and the error
/// keeps today's behaviour. Nothing is suppressed that this relay did not
/// itself sign.
fn replayed_nonce(message: &str) -> Option<&str> {
    if !message.contains("NonceAlreadyUsed") && !message.contains("Nonce has already been used") {
        return None;
    }
    let bytes = message.as_bytes();
    let is_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    // One pass for a maximal hex run of exactly the nonce's width; a longer
    // run is not a nonce and must not be truncated into one.
    let mut start = 0;
    while start < bytes.len() {
        if !is_hex(bytes[start]) {
            start += 1;
            continue;
        }
        let mut end = start;
        while end < bytes.len() && is_hex(bytes[end]) {
            end += 1;
        }
        if end - start == 32 {
            return Some(&message[start..end]);
        }
        start = end;
    }
    None
}

#[cfg(test)]
mod tests {
    /// GitLab #121: a command names its Beacon or Node, else the Hub it goes to.
    #[test]
    fn command_target_reads_the_device_off_the_params() {
        assert_eq!(
            command_target("hub1", &json!({ "mac": "AA:BB", "job": 1 })),
            "AA:BB"
        );
        assert_eq!(
            command_target("hub1", &json!({ "dev_eui": "0011", "to": "p2p" })),
            "0011"
        );
        assert_eq!(command_target("hub1", &json!({ "force": true })), "hub1");
        assert_eq!(command_target("hub1", &json!({ "mac": " " })), "hub1");
    }

    use super::*;
    use serde_json::json;
    use std::sync::Mutex as StdMutex;
    use time::format_description::well_known::Rfc3339;

    /// Captured `(topic, payload)` pairs shared between a `Publisher`
    /// double and the test asserting against it.
    type Captured = Arc<StdMutex<Vec<(String, Vec<u8>)>>>;

    /// A `Publisher` double that records every publish and never resolves
    /// (`std::future::pending`) — used to pin the "pending stored before
    /// publish" ordering without racing real time.
    #[derive(Default, Clone)]
    struct BlockingPublisher {
        sent: Captured,
    }

    impl Publisher for BlockingPublisher {
        fn publish(
            &self,
            topic: String,
            payload: Vec<u8>,
        ) -> BoxFuture<'static, anyhow::Result<()>> {
            self.sent.lock().unwrap().push((topic, payload));
            Box::pin(std::future::pending())
        }
    }

    /// A `Publisher` double that records every publish and resolves
    /// immediately with `Ok(())` — used for tests that need the relay's
    /// `execute`/`pair` calls to actually proceed.
    #[derive(Default, Clone)]
    struct RecordingPublisher {
        sent: Captured,
    }

    impl Publisher for RecordingPublisher {
        fn publish(
            &self,
            topic: String,
            payload: Vec<u8>,
        ) -> BoxFuture<'static, anyhow::Result<()>> {
            self.sent.lock().unwrap().push((topic, payload));
            Box::pin(async { Ok(()) })
        }
    }

    /// No-op publisher for tests that only need a `CommandRelay` to exist
    /// (e.g. wiring it into `AppState`) — never actually invoked.
    struct NoopPublisher;
    impl Publisher for NoopPublisher {
        fn publish(
            &self,
            _topic: String,
            _payload: Vec<u8>,
        ) -> BoxFuture<'static, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    /// A broker that cannot be reached: every publish fails.
    struct DownPublisher;
    impl Publisher for DownPublisher {
        fn publish(
            &self,
            _topic: String,
            _payload: Vec<u8>,
        ) -> BoxFuture<'static, anyhow::Result<()>> {
            Box::pin(async { Err(anyhow::anyhow!("broker unreachable")) })
        }
    }

    impl CommandRelay {
        /// Test-only convenience: a relay that never actually publishes
        /// anywhere, for tests (elsewhere in the crate) that only need a
        /// `CommandRelay` value to exist.
        pub fn test_instance() -> Self {
            Self::new(NoopPublisher, "proximos")
        }

        /// Test-only: a relay whose broker is down, so every command ends
        /// `mqtt_unavailable` without leaving the portal.
        pub fn broker_down_instance() -> Self {
            Self::new(DownPublisher, "proximos")
        }
    }

    fn request_id_of(body: &[u8]) -> String {
        let value: Value = serde_json::from_slice(body).expect("valid json");
        value["request_id"]
            .as_str()
            .expect("request_id present")
            .to_string()
    }

    /// A key minted the way `KeyService::ensure_user_key` mints one for a
    /// user of `role`: every permission `perms::permissions_for` gives that
    /// role, and the matching live ceiling.
    fn key_for(role: crate::auth::Role, is_superuser: bool) -> UserSigningKey {
        let user = crate::auth::CurrentUser {
            id: 1,
            username: "alice".to_string(),
            full_name: Some("Alice Doe".to_string()),
            user_role: role,
            is_superuser,
            monitoring_view_mode: "table-simplified".into(),
        };
        let seed = Zeroizing::new([9u8; 32]);
        let now = time::OffsetDateTime::now_utc();
        let payload = fiber_protocol::CertificatePayload {
            signer_id: "user-1-alice".to_string(),
            full_name: "Alice Doe".to_string(),
            role: role.as_str().to_string(),
            public_key_ed25519: fiber_protocol::derive_public_hex(&seed),
            permissions: crate::perms::permissions_for(&user),
            issued_at: now.format(&Rfc3339).unwrap(),
            expires_at: (now + time::Duration::days(1)).format(&Rfc3339).unwrap(),
            issuer: "device_ca".to_string(),
        };
        let ca_seed = [7u8; 32];
        let certificate_signature = fiber_protocol::sign_certificate(&ca_seed, &payload);
        let certificate = fiber_protocol::Certificate {
            payload,
            certificate_signature,
        };
        UserSigningKey {
            signer_id: "user-1-alice".to_string(),
            seed,
            certificate: serde_json::to_value(&certificate).unwrap(),
            ceiling: crate::perms::level_ceiling(&user),
        }
    }

    /// The default actor for the tests below: a manager, the role most of
    /// the portal's write routes gate on.
    fn test_key() -> UserSigningKey {
        key_for(crate::auth::Role::Manager, false)
    }

    async fn wait_until(mut cond: impl FnMut() -> bool) {
        for _ in 0..500 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("condition not met in time");
    }

    #[tokio::test]
    async fn execute_stores_pending_before_publish() {
        let publisher = BlockingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");

        let relay2 = relay.clone();
        // `restart_application` is `destructive` (design.md §4.2), so the
        // actor here is an admin — see
        // `a_manager_is_refused_a_destructive_verb_before_signing`.
        let key = key_for(crate::auth::Role::Admin, false);
        let _task = tokio::spawn(async move {
            relay2
                .execute(
                    "alice",
                    &key,
                    "FIBER-001",
                    "restart_application",
                    json!({}),
                    None,
                )
                .await
        });

        // execute_config_request runs entirely synchronously (insert into
        // `pending`, sign, serialize) up to the `.await` on `publish` —
        // one yield is enough to reach that suspension point.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let captured = sent.lock().unwrap().clone();
        assert_eq!(
            captured.len(),
            1,
            "publish must have been called exactly once"
        );
        assert_eq!(captured[0].0, "proximos/fiber-001/commands/config/request");
        let request_id = request_id_of(&captured[0].1);

        assert!(
            relay.pending.lock().contains_key(&request_id),
            "pending must already hold the entry by the time publish is called"
        );
    }

    #[tokio::test]
    async fn unknown_command_type_errors_without_publishing() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");
        let key = test_key();

        let outcome = relay
            .execute(
                "alice",
                &key,
                "FIBER-001",
                "not_a_real_command",
                json!({}),
                None,
            )
            .await;

        assert_eq!(outcome.status, "error");
        assert!(outcome.request_id.is_none());
        assert!(sent.lock().unwrap().is_empty(), "no fallback publish");
    }

    // ---- §7: the one publish chokepoint ----

    /// P2's last acceptance criterion, and decision #10's rule made
    /// checkable: **every** command leaves the portal through this module.
    ///
    /// A `grep`-shaped test over the source tree rather than a type-level
    /// guarantee, because what it forbids is a *string* — a second place
    /// formatting `{prefix}/{host}/commands/…` and handing it to a
    /// `Publisher`, which is exactly what the alarm ack and the backfill
    /// used to do (design.md §5.4). Neither a trait nor a visibility rule
    /// stops that; a test reading the sources does.
    #[test]
    fn no_command_topic_is_formatted_outside_this_module() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();

        fn walk(dir: &std::path::Path, offenders: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("readable source tree") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    walk(&path, offenders);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                // This module is the chokepoint, so it is the one file
                // allowed to build the topic.
                if path.file_name().and_then(|f| f.to_str()) == Some("relay.rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("readable source");
                for (i, line) in text.lines().enumerate() {
                    let code = line.trim_start();
                    // Doc comments and prose name the topics constantly;
                    // what matters is a format string in real code.
                    if code.starts_with("//") {
                        continue;
                    }
                    if code.contains("/commands/") && code.contains("format!") {
                        offenders.push(format!("{}:{}: {}", path.display(), i + 1, code.trim()));
                    }
                }
            }
        }

        walk(&src, &mut offenders);
        assert!(
            offenders.is_empty(),
            "a command topic is formatted outside relay.rs — every command must go \
             through `CommandRelay::execute` (design.md §7, decision #10):\n{}",
            offenders.join("\n")
        );
    }

    // ---- §6.2: redelivery ----

    /// P2's second acceptance criterion. **rumqttc replays unacked
    /// publishes on reconnect regardless of `clean_session`**, so the same
    /// `config/challenge` can arrive twice; the second must not produce a
    /// second confirm, which the device would answer with
    /// `ChallengeNotFound` and the operator would read as a failure.
    #[tokio::test]
    async fn a_redelivered_challenge_produces_one_confirm() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");
        let key = key_for(crate::auth::Role::Admin, false);

        let (tx, rx) = mpsc::unbounded_channel();
        let run_relay = relay.clone();
        tokio::spawn(async move { run_relay.run(rx).await });

        let execute_relay = relay.clone();
        let _task = tokio::spawn(async move {
            execute_relay
                .execute(
                    "alice",
                    &key,
                    "FIBER-001",
                    "restart_application",
                    json!({}),
                    None,
                )
                .await
        });

        let request_id = wait_for_request_id(&sent).await;
        let challenge = |request_id: String| IngestEvent::ConfigChallenge {
            hostname: "FIBER-001".into(),
            payload: ConfigChallengeMsg {
                challenge_id: "chal-dup".into(),
                request_id: Some(request_id),
                signer_id: None,
                expires_at: None,
                preview: None,
            },
        };

        tx.send(challenge(request_id.clone())).unwrap();
        wait_until(|| confirms(&sent) == 1).await;

        // The replay.
        tx.send(challenge(request_id)).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            confirms(&sent),
            1,
            "the replayed challenge is a debug line, never a second confirm"
        );
    }

    /// Duplicate-error tolerance (design.md §6.2): the other half of the
    /// replay story the seen-set above covers.
    ///
    /// rumqttc replays an unacked publish across a reconnect, so the device
    /// sees our `config_confirm` twice, verifies the first and refuses the
    /// second with `NonceAlreadyUsed` on the `errors` topic. The command
    /// succeeded; the error describes the duplicate. It must not reach the
    /// operator as a failed command.
    ///
    /// The nonce is read off the frame the relay actually published rather
    /// than invented, so this pins the ledger and the message parser
    /// together — a change to either that breaks the correlation fails here.
    #[tokio::test]
    async fn a_duplicate_nonce_error_for_a_known_request_is_ignored() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");
        let key = key_for(crate::auth::Role::Admin, false);

        let (tx, rx) = mpsc::unbounded_channel();
        let run_relay = relay.clone();
        tokio::spawn(async move { run_relay.run(rx).await });

        let execute_relay = relay.clone();
        let _task = tokio::spawn(async move {
            execute_relay
                .execute(
                    "alice",
                    &key,
                    "FIBER-001",
                    "restart_application",
                    json!({}),
                    None,
                )
                .await
        });

        let request_id = wait_for_request_id(&sent).await;
        tx.send(IngestEvent::ConfigChallenge {
            hostname: "FIBER-001".into(),
            payload: ConfigChallengeMsg {
                challenge_id: "chal-dup-err".into(),
                request_id: Some(request_id.clone()),
                signer_id: None,
                expires_at: None,
                preview: None,
            },
        })
        .unwrap();
        wait_until(|| confirms(&sent) == 1).await;

        // The nonce the relay signed the confirm with — the only token the
        // device's error payload shares with the frame we sent.
        let nonce = {
            let frames = sent.lock().unwrap();
            let (_, payload) = frames
                .iter()
                .find(|(topic, _)| topic.ends_with("commands/config/confirm"))
                .expect("a confirm reached the wire");
            let value: Value = serde_json::from_slice(payload).unwrap();
            value["nonce"].as_str().unwrap().to_string()
        };

        // Only statuses emitted from here on count: the "sent"/"challenge"/
        // "confirmed" three are the handshake doing its job.
        let mut statuses = relay.subscribe_status();

        tx.send(IngestEvent::DeviceError {
            hostname: "FIBER-001".into(),
            payload: DeviceErrorMsg {
                timestamp: Some("2026-09-15T09:00:00Z".into()),
                command: Some("config_confirm".into()),
                error: Some("authorization_failed".into()),
                // The device's exact wording: `AuthError`'s `Display` around
                // `CryptoError`'s `Debug` (`manager.rs`, `crypto/error.rs`).
                message: Some(format!(
                    "Signature verification failed: NonceAlreadyUsed(\"{nonce}\")"
                )),
            },
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        assert_eq!(
            confirms(&sent),
            1,
            "a device error is never answered with a second confirm"
        );
        assert!(
            statuses.try_recv().is_err(),
            "the duplicate is a debug line — no status change, and above all no `error`"
        );
    }

    /// The suppression is scoped to nonces this relay signed. An unrelated
    /// device error — including a nonce rejection for somebody else's frame
    /// — keeps today's behaviour, so the tolerance cannot become a blanket
    /// swallow of the `errors` topic.
    #[test]
    fn a_nonce_rejection_we_did_not_sign_is_not_recognised() {
        let relay = CommandRelay::new(RecordingPublisher::default(), "proximos");
        relay.remember_nonce("0123456789abcdef0123456789abcdef", "req-1");

        assert_eq!(
            replayed_nonce(
                "Signature verification failed: NonceAlreadyUsed(\"0123456789abcdef0123456789abcdef\")"
            ),
            Some("0123456789abcdef0123456789abcdef"),
            "the Debug rendering the device ships today"
        );
        assert_eq!(
            replayed_nonce(
                "Nonce has already been used (replay attack): 0123456789abcdef0123456789abcdef"
            ),
            Some("0123456789abcdef0123456789abcdef"),
            "and the Display rendering, should the device ever swap to it"
        );
        assert_eq!(
            replayed_nonce("Signature verification failed: InvalidSignature(\"bad\")"),
            None,
            "an unrelated failure is not a nonce rejection"
        );
        assert_eq!(
            replayed_nonce("NonceAlreadyUsed(\"deadbeef\")"),
            None,
            "a run that is not 32 hex characters is not a nonce"
        );
    }

    fn confirms(sent: &Captured) -> usize {
        sent.lock()
            .unwrap()
            .iter()
            .filter(|(topic, _)| topic.ends_with("commands/config/confirm"))
            .count()
    }

    async fn wait_for_request_id(sent: &Captured) -> String {
        for _ in 0..500 {
            if let Some((_, payload)) = sent.lock().unwrap().first() {
                let value: Value = serde_json::from_slice(payload).unwrap();
                return value["request_id"].as_str().unwrap().to_string();
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("no request reached the wire");
    }

    /// A single-shot `read`/`operate` never had a challenge, so its
    /// response resolves on the `request_id` the device echoes back
    /// (design.md §5.2).
    #[tokio::test]
    async fn a_single_shot_response_resolves_by_request_id() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");
        let key = test_key();

        let (tx, rx) = mpsc::unbounded_channel();
        let run_relay = relay.clone();
        tokio::spawn(async move { run_relay.run(rx).await });

        let execute_relay = relay.clone();
        let task = tokio::spawn(async move {
            execute_relay
                .execute(
                    "alice",
                    &key,
                    "FIBER-001",
                    "get_sticker_info",
                    json!({"dev_eui": "aabbccddeeff0011"}),
                    None,
                )
                .await
        });

        let request_id = wait_for_request_id(&sent).await;
        tx.send(IngestEvent::ConfigResponse {
            hostname: "FIBER-001".into(),
            payload: ConfigResponseMsg {
                challenge_id: "audit-key-only".into(),
                request_id: Some(request_id.clone()),
                status: "SUCCESS".into(),
                applied_at: None,
                effective_at: None,
                message: None,
                result: None,
            },
        })
        .unwrap();

        let outcome = task.await.expect("the execute task joins");
        assert_eq!(outcome.status, "success");
        assert_eq!(outcome.request_id.as_deref(), Some(request_id.as_str()));
        assert!(
            !sent
                .lock()
                .unwrap()
                .iter()
                .any(|(topic, _)| topic.ends_with("config/confirm")),
            "a single-shot never confirms"
        );
    }

    /// A non-success `config/response` is the device's own refusal: the
    /// outcome says so as a slug and keeps the device's text as the detail.
    #[tokio::test]
    async fn a_device_refusal_is_classified_as_rejected() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");
        let key = test_key();

        let (tx, rx) = mpsc::unbounded_channel();
        let run_relay = relay.clone();
        tokio::spawn(async move { run_relay.run(rx).await });

        let execute_relay = relay.clone();
        let task = tokio::spawn(async move {
            execute_relay
                .execute(
                    "alice",
                    &key,
                    "FIBER-001",
                    "get_sticker_info",
                    json!({"dev_eui": "aabbccddeeff0011"}),
                    None,
                )
                .await
        });

        let request_id = wait_for_request_id(&sent).await;
        tx.send(IngestEvent::ConfigResponse {
            hostname: "FIBER-001".into(),
            payload: ConfigResponseMsg {
                challenge_id: "audit-key-only".into(),
                request_id: Some(request_id),
                status: "ERROR".into(),
                applied_at: None,
                effective_at: None,
                message: Some("unknown sticker".into()),
                result: None,
            },
        })
        .unwrap();

        let outcome = task.await.expect("the execute task joins");
        assert_eq!(outcome.status, "error");
        assert_eq!(outcome.error_code, Some(error_code::DEVICE_REJECTED));
        assert_eq!(outcome.message.as_deref(), Some("unknown sticker"));
    }

    // ---- §4.4: refuse before signing ----

    /// The matrix design.md §15 asks for: five levels × three roles, and
    /// nothing reaches the wire unless the caller's role reaches the level.
    ///
    /// The verbs are one per level, each chosen so its **permission** is
    /// held by every role that could plausibly try it — the point being
    /// that what stops a manager is the level, not a missing permission.
    #[tokio::test]
    async fn refuse_before_sign_covers_every_level_and_role() {
        use crate::auth::Role;
        use fiber_protocol::Level;

        // (verb, level) — one per rung.
        let verbs = [
            ("get_sticker_info", Level::Read),
            ("silence_buzzer", Level::Operate),
            ("set_threshold", Level::Manage),
            ("add_signer", Level::Admin),
            ("factory_reset", Level::Destructive),
        ];
        let roles = [
            (Role::Viewer, Level::Read),
            (Role::Manager, Level::Manage),
            (Role::Admin, Level::Destructive),
        ];

        for (role, ceiling) in roles {
            for (command_type, level) in verbs {
                let publisher = RecordingPublisher::default();
                let sent = publisher.sent.clone();
                let relay = CommandRelay::new(publisher, "proximos");
                let key = key_for(role, false);

                let outcome = tokio::time::timeout(
                    Duration::from_millis(50),
                    relay.execute("alice", &key, "FIBER-001", command_type, json!({}), None),
                )
                .await;

                if level <= ceiling {
                    assert!(
                        outcome.is_err(),
                        "{role:?}/{command_type}: allowed, so it should be waiting on a device"
                    );
                    assert_eq!(
                        sent.lock().unwrap().len(),
                        1,
                        "{role:?}/{command_type}: allowed, so exactly one publish"
                    );
                } else {
                    let outcome = outcome.expect("a refusal returns immediately");
                    assert_eq!(outcome.status, "error", "{role:?}/{command_type}");
                    assert!(
                        outcome
                            .message
                            .as_deref()
                            .is_some_and(|m| m.starts_with("level_denied")),
                        "{role:?}/{command_type}: {:?}",
                        outcome.message
                    );
                    assert!(
                        outcome.request_id.is_none(),
                        "{role:?}/{command_type}: nothing was ever signed"
                    );
                    assert!(
                        sent.lock().unwrap().is_empty(),
                        "{role:?}/{command_type}: refused before signing means nothing on the wire"
                    );
                }
            }
        }
    }

    /// The case the level exists for (design.md §4.3): a manager's
    /// certificate carries `set_lorawan_sensor_config`, which is also
    /// `sticker_device_reset`'s permission — so a permission check alone
    /// would let this through, and only the level stops it.
    #[tokio::test]
    async fn a_manager_is_refused_a_destructive_verb_before_signing() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");
        let key = test_key();

        assert!(
            key.certificate["permissions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p == "set_lorawan_sensor_config"),
            "the premise: the manager does hold the verb's own permission"
        );

        let outcome = relay
            .execute(
                "alice",
                &key,
                "FIBER-001",
                "sticker_device_reset",
                json!({"dev_eui": "aabbccddeeff0011"}),
                None,
            )
            .await;

        assert_eq!(outcome.status, "error");
        assert_eq!(
            outcome.message.as_deref(),
            Some("level_denied: sticker_device_reset is destructive")
        );
        assert_eq!(outcome.error_code, Some(error_code::NOT_PERMITTED));
        assert!(sent.lock().unwrap().is_empty());
    }

    /// A certificate that predates a permission is refused with
    /// `permission_missing` rather than published and rejected by the
    /// device later and less helpfully — v1's behaviour for every verb
    /// outside the hand-picked preflight quartet.
    #[tokio::test]
    async fn a_certificate_missing_the_verbs_permission_is_refused() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");

        let mut key = test_key();
        key.certificate["permissions"] = json!(["level:read", "level:operate", "level:manage"]);

        let outcome = relay
            .execute("alice", &key, "FIBER-001", "set_threshold", json!({}), None)
            .await;
        assert_eq!(
            outcome.message.as_deref(),
            Some("permission_missing: set_threshold")
        );
        assert!(sent.lock().unwrap().is_empty());
    }

    /// The union re-mint never removes a permission, so a demoted user's
    /// certificate still carries `level:destructive`. The live role is what
    /// decides — which is why the ceiling rides on the key rather than
    /// being read back off the certificate (design.md §4.4 step 2).
    #[tokio::test]
    async fn a_stale_certificate_cannot_outrank_the_live_role() {
        use fiber_protocol::Level;

        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");

        // What `ensure_user_key`'s union leaves behind after a demotion.
        let mut key = key_for(crate::auth::Role::Admin, false);
        assert!(key.certificate["permissions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "level:destructive"));
        key.ceiling = Level::Manage; // demoted to manager

        let outcome = relay
            .execute("alice", &key, "FIBER-001", "factory_reset", json!({}), None)
            .await;
        assert_eq!(
            outcome.message.as_deref(),
            Some("level_denied: factory_reset is destructive")
        );
        assert!(sent.lock().unwrap().is_empty());
    }

    /// No registry row, no publish — v1's `reset_export_cursor` defect
    /// (mqtt.md quirk #3) — and the slug says which of the three refusals
    /// it was.
    #[tokio::test]
    async fn an_unknown_verb_is_refused_with_its_own_slug() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");
        let key = key_for(crate::auth::Role::Admin, false);

        let outcome = relay
            .execute("alice", &key, "FIBER-001", "add_eye_tag", json!({}), None)
            .await;
        assert_eq!(
            outcome.message.as_deref(),
            Some("unknown_command: add_eye_tag"),
            "none of the v1 *_eye_* names exists in v2"
        );
        assert_eq!(outcome.error_code, None, "a portal bug, not an operator's");
        assert!(sent.lock().unwrap().is_empty(), "no fallback publish");
    }

    /// #470 H4: the Hub's `AddressCollision` line gets its own slug, any other refusal stays
    /// `device_rejected`, the prose after the kind decides nothing.
    #[test]
    fn a_hub_address_collision_is_classified_by_its_kind() {
        let line = "AddressCollision [p2p/node-registry]: dev_eui 58760700d0000abe has the P2P \
                    address d0000abe (its low 32 bits), which 70b3d57ed0000abe already holds on p2p";
        assert_eq!(hub_refusal_code(Some(line)), error_code::ADDRESS_COLLISION);
        for other in [
            Some("AlreadyExists [p2p/node-registry]: AddressCollision elsewhere"),
            Some("legacy LoRaWAN stack disabled"),
            Some(""),
            None,
        ] {
            assert_eq!(
                hub_refusal_code(other),
                error_code::DEVICE_REJECTED,
                "{other:?}"
            );
        }
    }

    /// Every slug a failure can carry has its sentence in the catalogue
    /// (`translation_keys_match_across_languages` then holds `cs` to it).
    #[test]
    fn every_command_error_slug_has_a_sentence() {
        for slug in error_code::ALL {
            let key = format!("monitoring.command_error.{slug}");
            assert_ne!(crate::i18n::lookup("en", &key), key, "{slug}");
        }
    }

    #[test]
    fn the_refusal_slugs_are_the_ones_ui_m2_names() {
        use fiber_protocol::Level;
        assert_eq!(Refusal::UnknownCommand.slug(), "unknown_command");
        assert_eq!(Refusal::LevelDenied(Level::Admin).slug(), "level_denied");
        assert_eq!(
            Refusal::PermissionMissing("x".into()).slug(),
            "permission_missing"
        );
    }

    /// The eight v1 unsigned flat-dialect verbs are ordinary signed rows
    /// now: one topic, one dialect (design.md §7). `silence_buzzer` used to
    /// go out as a bare `{"command": "silence_buzzer"}` on its own topic
    /// with no permission at all — which is what let a read-only
    /// certificate silence a medical alarm.
    #[tokio::test]
    async fn an_ex_plain_verb_publishes_a_signed_envelope_on_the_one_topic() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");
        let key = test_key();

        // No device answers, so this resolves on the timeout — the publish
        // is what the test is about.
        tokio::time::timeout(
            Duration::from_millis(50),
            relay.execute(
                "alice",
                &key,
                "FIBER-001",
                "silence_buzzer",
                json!({}),
                None,
            ),
        )
        .await
        .ok();

        let captured = sent.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0, "proximos/fiber-001/commands/config/request");
        let body: Value = serde_json::from_slice(&captured[0].1).unwrap();
        assert_eq!(body["command"], "config_request");
        assert_eq!(body["command_type"], "silence_buzzer");
        assert_eq!(body["v"], 2);
        assert_eq!(
            body["level"], "operate",
            "from the registry, not the caller"
        );
        assert!(body["signature"].as_str().is_some_and(|s| !s.is_empty()));
    }

    /// Params ride inside the signed `params` object rather than being
    /// merged into the top level as the flat dialect did.
    #[tokio::test]
    async fn params_ride_inside_the_signed_envelope() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");
        let key = test_key();

        tokio::time::timeout(
            Duration::from_millis(50),
            relay.execute(
                "alice",
                &key,
                "FIBER-001",
                "get_sticker_config",
                json!({"dev_eui": "aabb"}),
                None,
            ),
        )
        .await
        .ok();

        let captured = sent.lock().unwrap();
        let body: Value = serde_json::from_slice(&captured[0].1).unwrap();
        assert_eq!(body["command_type"], "get_sticker_config");
        assert_eq!(body["level"], "read");
        assert_eq!(body["params"], json!({"dev_eui": "aabb"}));
        assert_eq!(body["reason"], "", "a string, never null");
    }

    #[tokio::test]
    async fn challenge_for_unknown_request_warns_without_publish() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");

        let (tx, rx) = mpsc::unbounded_channel();
        let run_relay = relay.clone();
        tokio::spawn(async move { run_relay.run(rx).await });

        tx.send(IngestEvent::ConfigChallenge {
            hostname: "FIBER-001".into(),
            payload: ConfigChallengeMsg {
                challenge_id: "chal-orphan".into(),
                request_id: Some("no-such-request".into()),
                signer_id: None,
                expires_at: None,
                preview: None,
            },
        })
        .unwrap();

        // Give the run() task a chance to process, then confirm nothing
        // was published.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn full_challenge_confirm_response_happy_path() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");

        let (relay_tx, relay_rx) = mpsc::unbounded_channel();
        let run_relay = relay.clone();
        tokio::spawn(async move { run_relay.run(relay_rx).await });

        let mut status_rx = relay.subscribe_status();

        // `restart_application` is `destructive` (design.md §4.2), so the
        // actor here is an admin — see
        // `a_manager_is_refused_a_destructive_verb_before_signing`.
        let key = key_for(crate::auth::Role::Admin, false);
        let exec_relay = relay.clone();
        let exec_task = tokio::spawn(async move {
            exec_relay
                .execute(
                    "alice",
                    &key,
                    "FIBER-001",
                    "restart_application",
                    json!({}),
                    None,
                )
                .await
        });

        wait_until(|| !sent.lock().unwrap().is_empty()).await;
        let request_id = {
            let captured = sent.lock().unwrap();
            request_id_of(&captured[0].1)
        };

        relay_tx
            .send(IngestEvent::ConfigChallenge {
                hostname: "FIBER-001".into(),
                payload: ConfigChallengeMsg {
                    challenge_id: "chal-1".into(),
                    request_id: Some(request_id.clone()),
                    signer_id: None,
                    expires_at: None,
                    preview: None,
                },
            })
            .unwrap();

        wait_until(|| sent.lock().unwrap().len() >= 2).await;
        {
            let captured = sent.lock().unwrap();
            assert_eq!(captured[1].0, "proximos/fiber-001/commands/config/confirm");
        }

        relay_tx
            .send(IngestEvent::ConfigResponse {
                hostname: "FIBER-001".into(),
                payload: ConfigResponseMsg {
                    challenge_id: "chal-1".into(),
                    request_id: None,
                    status: "SUCCESS".into(),
                    applied_at: None,
                    effective_at: None,
                    message: Some("Rebooted".into()),
                    result: None,
                },
            })
            .unwrap();

        let outcome = exec_task.await.unwrap();
        assert_eq!(outcome.status, "success");
        assert_eq!(outcome.message.as_deref(), Some("Rebooted"));
        assert_eq!(outcome.request_id.as_deref(), Some(request_id.as_str()));

        let frames: Vec<CommandStatus> = {
            let mut out = Vec::new();
            while let Ok(s) = status_rx.try_recv() {
                out.push(s);
            }
            out
        };
        let statuses: Vec<&'static str> = frames.iter().map(|s| s.status).collect();
        assert_eq!(statuses, vec!["sent", "challenge", "confirmed", "success"]);
        // GitLab #121: every frame names the device, here the Hub itself.
        assert!(frames
            .iter()
            .all(|s| s.target.as_deref() == Some("FIBER-001")));
    }

    /// C5: `cluster-client-add` answers with a `result` that holds a Client
    /// Hub's broker password (plan/control/cluster.md §7). It reaches the
    /// caller of `execute` — the Add-hub flow, which shows it once — and no
    /// status transition, which is what the SSE hub renders.
    #[tokio::test]
    async fn a_verbs_result_reaches_its_caller_and_no_status_event() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");

        let (relay_tx, relay_rx) = mpsc::unbounded_channel();
        let run_relay = relay.clone();
        tokio::spawn(async move { run_relay.run(relay_rx).await });
        let mut status_rx = relay.subscribe_status();

        let key = key_for(crate::auth::Role::Admin, false);
        let exec_relay = relay.clone();
        let exec_task = tokio::spawn(async move {
            exec_relay
                .execute(
                    "alice",
                    &key,
                    "proximos-a1b2c3d4",
                    "cluster-client-add",
                    json!({"hostname": "proximos-1a2b3c4d"}),
                    None,
                )
                .await
        });

        wait_until(|| !sent.lock().unwrap().is_empty()).await;
        let request_id = request_id_of(&sent.lock().unwrap()[0].1);
        relay_tx
            .send(IngestEvent::ConfigChallenge {
                hostname: "proximos-a1b2c3d4".into(),
                payload: ConfigChallengeMsg {
                    challenge_id: "chal-c5".into(),
                    request_id: Some(request_id.clone()),
                    signer_id: None,
                    expires_at: None,
                    preview: None,
                },
            })
            .unwrap();
        wait_until(|| sent.lock().unwrap().len() >= 2).await;

        let password = "pw-pw-pw-pw-pw-pw-pw-pw";
        relay_tx
            .send(IngestEvent::ConfigResponse {
                hostname: "proximos-a1b2c3d4".into(),
                payload: ConfigResponseMsg {
                    challenge_id: "chal-c5".into(),
                    request_id: None,
                    status: "ERROR".into(),
                    applied_at: None,
                    effective_at: None,
                    message: Some(
                        "Execution failed: registration incomplete: gateway failed".into(),
                    ),
                    result: Some(ResponseResult(json!({
                        "hostname": "proximos-1a2b3c4d",
                        "account": "ok",
                        "gateway": "failed",
                        "password": password,
                    }))),
                },
            })
            .unwrap();

        let outcome = exec_task.await.unwrap();
        assert_eq!(
            outcome.status, "error",
            "a partial result is still an error"
        );
        let result = outcome
            .result
            .as_ref()
            .expect("the result rides on an error too");
        assert_eq!(result.0["password"], password);
        assert!(
            !format!("{outcome:?}").contains(password),
            "a debug print of the outcome shows keys, not values"
        );

        let mut statuses = Vec::new();
        while let Ok(s) = status_rx.try_recv() {
            assert!(
                !format!("{s:?}").contains(password),
                "no status event carries the password: {s:?}"
            );
            statuses.push(s.status);
        }
        assert_eq!(statuses, vec!["sent", "challenge", "confirmed", "error"]);
    }

    // ---- `dispatch`: published, not awaited ----

    /// The whole point of [`CommandRelay::dispatch`]: it comes back once the
    /// frame is on the wire, so a caller with nobody waiting on the device
    /// — the alarm ack, backfill's drain — never pays `COMMAND_TIMEOUT`.
    ///
    /// Nothing answers before the assertion, which is what makes it an
    /// assertion: an `execute` here would still be parked 20 s later.
    #[tokio::test]
    async fn dispatch_returns_sent_without_waiting_for_the_device() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");
        let key = test_key();

        let (tx, rx) = mpsc::unbounded_channel();
        let run_relay = relay.clone();
        tokio::spawn(async move { run_relay.run(rx).await });
        let mut status_rx = relay.subscribe_status();

        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            relay.dispatch(
                "alice",
                &key,
                "FIBER-001",
                "silence_buzzer",
                json!({}),
                None,
            ),
        )
        .await
        .expect("dispatch must not wait out the command timeout");

        assert_eq!(outcome.status, "sent");
        let request_id = outcome.request_id.clone().expect("a correlatable request");
        {
            let captured = sent.lock().unwrap();
            assert_eq!(captured.len(), 1, "published before returning");
            assert_eq!(captured[0].0, "proximos/fiber-001/commands/config/request");
            assert_eq!(request_id_of(&captured[0].1), request_id);
        }

        // The detached half is still listening: the device's answer lands on
        // the status broadcast exactly as it would have under `execute`, so
        // the SSE hub cannot tell the two entry points apart.
        tx.send(IngestEvent::ConfigResponse {
            hostname: "FIBER-001".into(),
            payload: ConfigResponseMsg {
                challenge_id: "audit-key-only".into(),
                request_id: Some(request_id.clone()),
                status: "SUCCESS".into(),
                applied_at: None,
                effective_at: None,
                message: None,
                result: None,
            },
        })
        .unwrap();

        let mut statuses = Vec::new();
        wait_until(|| {
            while let Ok(s) = status_rx.try_recv() {
                statuses.push(s.status);
            }
            statuses.contains(&"success")
        })
        .await;
        assert_eq!(statuses, vec!["sent", "success"]);
    }

    /// A refusal is the one thing `dispatch` still answers synchronously:
    /// §4.4's gate runs before anything is signed, so "the portal would not
    /// send this" stays distinguishable from "the portal sent it" — which is
    /// what `backfill` keys its retry on and what keeps a Viewer's ack from
    /// reporting a silence that never happened.
    #[tokio::test]
    async fn dispatch_still_refuses_before_signing_and_publishes_nothing() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");
        let key = key_for(crate::auth::Role::Manager, false);

        let outcome = relay
            .dispatch(
                "alice",
                &key,
                "FIBER-001",
                "sticker_device_reset",
                json!({}),
                None,
            )
            .await;

        assert_eq!(outcome.status, "error");
        assert_eq!(
            outcome.message.as_deref(),
            Some("level_denied: sticker_device_reset is destructive")
        );
        assert!(outcome.request_id.is_none(), "nothing was signed");
        assert!(sent.lock().unwrap().is_empty(), "nothing was published");
    }

    #[tokio::test(start_paused = true)]
    async fn execute_config_request_times_out_without_a_response() {
        let publisher = RecordingPublisher::default();
        let relay = CommandRelay::new(publisher, "proximos");
        // `restart_application` is `destructive` (design.md §4.2), so the
        // actor here is an admin — see
        // `a_manager_is_refused_a_destructive_verb_before_signing`.
        let key = key_for(crate::auth::Role::Admin, false);

        let relay2 = relay.clone();
        let task = tokio::spawn(async move {
            relay2
                .execute(
                    "alice",
                    &key,
                    "FIBER-001",
                    "restart_application",
                    json!({}),
                    None,
                )
                .await
        });

        tokio::task::yield_now().await;
        tokio::time::advance(COMMAND_TIMEOUT + Duration::from_secs(1)).await;

        let outcome = task.await.unwrap();
        assert_eq!(outcome.status, "timeout");
        assert_eq!(outcome.error_code, Some(error_code::NO_ANSWER));
        assert!(
            relay.pending.lock().is_empty(),
            "timed-out entry must be cleaned up"
        );
    }

    /// axum drops the handler future when the HTTP client disconnects. Until
    /// the 2026-09-15 review, `await_outcome` ran inline in that future, so a
    /// dropped `execute` never reached its timeout arm and the `pending`
    /// entry — seed and certificate included — stayed for the life of the
    /// process. The wait is a spawned task now, so the sweep happens whether
    /// or not anyone is still listening.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_execute_still_sweeps_its_pending_entry() {
        let publisher = RecordingPublisher::default();
        let relay = CommandRelay::new(publisher, "proximos");
        let key = key_for(crate::auth::Role::Admin, false);

        let relay2 = relay.clone();
        let caller = tokio::spawn(async move {
            relay2
                .execute(
                    "alice",
                    &key,
                    "FIBER-001",
                    "restart_application",
                    json!({}),
                    None,
                )
                .await
        });
        wait_until(|| relay.pending.lock().len() == 1).await;

        // The client went away: the handler future is dropped mid-wait.
        caller.abort();
        let _ = caller.await;
        assert_eq!(
            relay.pending.lock().len(),
            1,
            "nothing has timed out yet — the entry is still legitimately pending"
        );

        tokio::time::advance(COMMAND_TIMEOUT + Duration::from_secs(1)).await;
        wait_until(|| relay.pending.lock().is_empty()).await;
    }

    #[tokio::test]
    async fn pairing_roundtrip() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");

        let (relay_tx, relay_rx) = mpsc::unbounded_channel();
        let run_relay = relay.clone();
        tokio::spawn(async move { run_relay.run(relay_rx).await });

        let pair_relay = relay.clone();
        let pair_task =
            tokio::spawn(async move { pair_relay.pair("FIBER-001", "admin", false).await });

        wait_until(|| !sent.lock().unwrap().is_empty()).await;
        let request_id = {
            let captured = sent.lock().unwrap();
            assert_eq!(captured[0].0, "proximos/fiber-001/pair/request");
            let body: serde_json::Value =
                serde_json::from_slice(&captured[0].1).expect("json body");
            assert_eq!(
                body.get("local"),
                Some(&serde_json::Value::Bool(false)),
                "`local` is always on the wire, not omitted for the code flow"
            );
            request_id_of(&captured[0].1)
        };

        relay_tx
            .send(IngestEvent::PairResponse {
                hostname: "FIBER-001".into(),
                payload: PairResponseMsg {
                    request_id,
                    success: true,
                    encrypted_private_key: None,
                    ca_public_key: Some("aa".into()),
                    ca_id: Some("ca-1".into()),
                    admin_certificate: None,
                    error: None,
                },
            })
            .unwrap();

        let result = pair_task.await.unwrap();
        let payload = result.expect("pairing succeeds");
        assert!(payload.success);
        assert_eq!(payload.ca_id.as_deref(), Some("ca-1"));
    }

    #[tokio::test(start_paused = true)]
    async fn local_pairing_sets_the_flag_on_the_wire() {
        // The device switches encryption password on this field alone, so a
        // dropped `local: true` silently costs the operator a pairing.
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = CommandRelay::new(publisher, "proximos");

        let relay2 = relay.clone();
        let task = tokio::spawn(async move { relay2.pair("FIBER-001", "admin", true).await });

        wait_until(|| !sent.lock().unwrap().is_empty()).await;
        {
            let captured = sent.lock().unwrap();
            let body: serde_json::Value =
                serde_json::from_slice(&captured[0].1).expect("json body");
            assert_eq!(body.get("local"), Some(&serde_json::Value::Bool(true)));
        }

        tokio::time::advance(PAIR_TIMEOUT + Duration::from_secs(1)).await;
        let _ = task.await;
    }

    #[tokio::test(start_paused = true)]
    async fn pairing_times_out_without_a_response() {
        let publisher = RecordingPublisher::default();
        let relay = CommandRelay::new(publisher, "proximos");

        let relay2 = relay.clone();
        let task = tokio::spawn(async move { relay2.pair("FIBER-001", "admin", false).await });

        tokio::task::yield_now().await;
        tokio::time::advance(PAIR_TIMEOUT + Duration::from_secs(1)).await;

        let result = task.await.unwrap();
        assert!(matches!(result, Err(RelayError::Timeout)));
    }
}
