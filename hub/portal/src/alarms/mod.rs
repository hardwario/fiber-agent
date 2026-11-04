//! `AlarmService` (M6-E): the alarm-read/ack/note API surface `routes::` (a
//! parallel work package) merges into the router, plus the SSE nudge
//! broadcast the persist thread and this service share, plus the ack side
//! effect (`silence_buzzer`).
//!
//! Owns SQLite connection #6 on the shared `data_dir`/`db_key` (after
//! persist, auth, read-db, keys, backfill) — another connection under WAL
//! is fine, same reasoning as every other service struct in this binary.
//! Access goes through `crate::db::Db`, like every other service here.
//!
//! `open()` deliberately takes only `(data_dir, db_key)` — the command
//! relay (ack's `silence_buzzer` side effect), the key service it signs
//! with, and the notifier's `NotifyJob` sender are wired in afterwards via
//! [`AlarmService::wire`], keeping `open` a plain two-arg constructor other
//! work packages (tests, future callers) can use without reaching for a
//! live/fake MQTT client.
//!
//! The notifier task and `NotifyJob` type live in [`notifier`] — see that
//! module for the consume/gate/send/system-alert side of M6-E.

pub mod notifier;
pub mod orphans;

use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use portal_domain::alarm_engine::{ack_validation, TextProblem};
use portal_storage::{
    AlarmCounts, AlarmEvent, AlarmListFilter, AlarmNote, SpillGate, Storage, StorageError,
};

use crate::auth::CurrentUser;
use crate::keys::KeyService;
use crate::relay::CommandRelay;

pub use notifier::{spawn_notifier, NotifyJob};

/// Which device an alarm belongs to, read off the `sensor_id` convention the
/// engine writes (`line-N` / `lrw:*` / `p2p:*` / `eye:*` / `sys:*`). The one
/// parser of that convention for display: links, names and the peak query
/// all start here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlarmTarget {
    /// A wired probe line on the alarm's Hub.
    Line(i64),
    /// A Node, by its `node_key` (DevEUI), either transport.
    Node(String),
    /// A Beacon, by MAC.
    Beacon(String),
    /// The Hub itself (`sys:*`), or anything unrecognised.
    Hub,
}

pub fn alarm_target(sensor_id: &str) -> AlarmTarget {
    let entity = |rest: &str| {
        rest.split(':')
            .next()
            .filter(|e| !e.is_empty())
            .map(str::to_string)
    };
    if let Some(line) = sensor_id.strip_prefix("line-").and_then(|n| n.parse().ok()) {
        return AlarmTarget::Line(line);
    }
    if let Some(key) = sensor_id
        .strip_prefix("lrw:")
        .or_else(|| sensor_id.strip_prefix("p2p:"))
        .and_then(entity)
    {
        return AlarmTarget::Node(key);
    }
    // A MAC carries colons of its own (`eye:AA:BB:CC:DD:EE:FF:Temperature`),
    // so a Beacon's entity is everything before the *last* one.
    match sensor_id
        .strip_prefix("eye:")
        .map(|rest| rest.rsplit_once(':').map_or(rest, |(mac, _)| mac))
        .filter(|mac| !mac.is_empty())
        .map(str::to_string)
    {
        Some(mac) => AlarmTarget::Beacon(mac),
        None => AlarmTarget::Hub,
    }
}

/// Whether the live registry knows the Hub `hostname` (decisions.md #30d).
/// The registry is rebuilt from retained messages, which the Hub's broker
/// does not persist, so a Hub can be missing from it while its alarm rows
/// stay open: those read "state unknown", are not counted, and link nowhere.
pub fn hub_known(state: &crate::state::PortalState, hostname: &str) -> bool {
    let wanted = hostname.trim();
    state
        .devices
        .values()
        .any(|d| d.hostname.trim().eq_ignore_ascii_case(wanted))
}

/// The name of the Node or Beacon an alarm belongs to, when the snapshot
/// knows one — a field alarm's `sensor_name` is the field's label, not the
/// device's name. A wired probe's `sensor_name` already is its name.
pub fn alarm_device_name(sensor_id: &str, state: &crate::state::PortalState) -> Option<String> {
    match alarm_target(sensor_id) {
        AlarmTarget::Node(key) => state.hub_of_node(&key.to_lowercase())?.1.name.clone(),
        AlarmTarget::Beacon(mac) => state.hub_of_beacon(&mac)?.1.name.clone(),
        AlarmTarget::Line(_) | AlarmTarget::Hub => None,
    }
}

/// What an alarm measures: the registry field name (`common.field.<name>`
/// is its label) and its unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlarmField {
    pub name: &'static str,
    pub unit: Option<&'static str>,
}

impl AlarmField {
    /// The unit an alarm row, detail, toast or e-mail shows: a Node's Pa
    /// reads as hPa (UX-038), as everywhere else in the portal. `unit` stays
    /// the stored unit, which the webhook carries.
    pub fn display_unit(self) -> Option<&'static str> {
        display_def(self.name, self.unit).map_or(self.unit, |f| f.display_unit())
    }

    /// A stored value (reading, limit, peak) in [`Self::display_unit`].
    pub fn to_display(self, stored: f64) -> f64 {
        display_def(self.name, self.unit).map_or(stored, |f| f.to_display(stored))
    }
}

/// The Node registry entry that owns the display conversion of a field
/// stored in `unit` (`LorawanFieldDef::to_display`), if any.
pub(crate) fn display_def(
    name: &str,
    unit: Option<&str>,
) -> Option<&'static fiber_protocol::LorawanFieldDef> {
    fiber_protocol::lorawan_field(name).filter(|f| f.unit == unit)
}

/// The measured quantity of an alarm, read off the `sensor_id` convention
/// the engine writes: `line-N` is a wired temperature probe, a Node or
/// Beacon field alarm ends in the field's registry label
/// (`lrw:<key>:Temperature`), and a native fPort-3 alarm
/// (`lrw:<key>:native:<slot>`) carries its field name as `sensor_name`.
/// `None` for alarms about a device rather than a reading (`sys:*`,
/// liveness), which have no value to put a unit on. Works for rows written
/// before anyone asked, because the convention is as old as the engine.
pub fn alarm_field(sensor_id: &str, sensor_name: Option<&str>) -> Option<AlarmField> {
    use fiber_protocol::{eye_registry, lorawan_field, lorawan_registry};
    if sensor_id.starts_with("line-") {
        return Some(AlarmField {
            name: "temperature",
            unit: Some("\u{00b0}C"),
        });
    }
    let node = sensor_id
        .strip_prefix("lrw:")
        .or_else(|| sensor_id.strip_prefix("p2p:"));
    if let Some(rest) = node {
        if rest.contains(":native:") {
            // A v2 Node's own alarm is named "Node · Temperature" (nodes_phase2.cs.md P2);
            // older rows carry the bare field name.
            let name = sensor_name?;
            let tail = name.rsplit(" \u{b7} ").next().unwrap_or(name);
            let f = lorawan_field(name)
                .or_else(|| lorawan_field(&tail.to_ascii_lowercase().replace(' ', "_")))
                .or_else(|| lorawan_registry().iter().find(|f| f.label == tail))?;
            return Some(AlarmField {
                name: f.name,
                unit: f.unit,
            });
        }
        let label = rest.split_once(':')?.1;
        let f = lorawan_registry().iter().find(|f| f.label == label)?;
        return Some(AlarmField {
            name: f.name,
            unit: f.unit,
        });
    }
    let label = sensor_id.strip_prefix("eye:")?.rsplit_once(':')?.1;
    let f = eye_registry().iter().find(|f| f.label == label)?;
    Some(AlarmField {
        name: f.name,
        unit: f.unit,
    })
}

/// SSE nudge broadcast capacity — same reasoning as every other nudge/change
/// channel in this binary (a lagging subscriber just re-renders on the next
/// nudge).
const NUDGE_CHANNEL_CAPACITY: usize = 256;

/// The ack side effect. In v1 this was an unsigned flat-dialect publish on
/// its own topic with no permission mapping at all — the bypass
/// plan/portal/mqtt/design.md §5.4 closes. It is a signed `operate` command
/// through the relay now, signed with the **acking user's** key, so the
/// Hub's audit trail records who silenced the alarm. v1 never recorded it.
const SILENCE_BUZZER: &str = "silence_buzzer";

/// "This hostname's alarm table is stale — re-render it." Broadcast on every
/// alarm mutation (open/severity/state/clear/ack/note); carries no row data
/// because the SSE layer re-reads `AlarmService::list` (same coarse-signal
/// philosophy as `state::ChangeEvent`/`backfill::BackfillNudge`).
#[derive(Debug, Clone)]
pub struct AlarmNudge {
    pub hostname: String,
    /// Ids of alarm rows *opened* by the mutation this nudge reports —
    /// `sse.rs` renders one activation pop-up (`partials/alarm_toast.html`)
    /// per id, re-reading the row itself (the coarse-signal philosophy
    /// above: ids only, no row data). Empty for every other mutation
    /// (ack/clear/severity/state/note).
    pub activated_ids: Vec<String>,
    /// Ids of alarm rows *escalated to critical* (warning → critical
    /// `UpdateSeverity`) by this mutation — same pop-up path as
    /// `activated_ids`, rendered with the escalation label. De-escalations
    /// (critical → warning) stay silent: the situation is improving, and a
    /// pop-up for that would read as a new problem.
    pub escalated_ids: Vec<String>,
}

/// Errors surfaced by [`AlarmService`].
#[derive(Debug, thiserror::Error)]
pub enum AlarmError {
    #[error("alarm not found")]
    NotFound,
    /// Operator-supplied text (ack reason, note) rejected — carries a slug
    /// the route turns into an `alarms.errors.*` catalogue key.
    #[error(transparent)]
    InvalidText(#[from] TextProblem),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// Result of a successful [`AlarmService::acknowledge`] call. The current
/// route caller (`routes::alarm_routes::acknowledge`) re-fetches the detail
/// view instead of consuming this directly, so its fields are unread today
/// — kept anyway because they're this method's documented contract (M6-E
/// work order: "returns the updated row + whether ack-notify should fire")
/// for any future/other caller (e.g. a JSON API route).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct AckOutcome {
    pub alarm: AlarmEvent,
    pub note: AlarmNote,
    /// `true` only on the *first* ack of this alarm (idempotence: a re-ack
    /// overwrites the stamp per `portal_storage::acknowledge_alarm`'s doc,
    /// but must not re-fire the acknowledged-notification every time).
    pub should_notify: bool,
}

/// The alarm read/ack/note API, plus the SSE nudge sender the persist thread
/// also holds (see `nudge_sender`). Cheap to clone (`Arc`-backed throughout)
/// — the same pattern every other service struct in this binary uses for
/// cloning into `AppState` per request.
#[derive(Clone)]
pub struct AlarmService {
    db: crate::db::Db,
    nudge_tx: broadcast::Sender<AlarmNudge>,
    /// Wired post-construction via [`Self::wire`]; `None` until then (or in
    /// a bare `open()`-only test instance) — the ack side effect and the
    /// notifier hookup both degrade to "log and skip" rather than panic.
    relay: Option<CommandRelay>,
    keys: Option<KeyService>,
    notify_tx: Option<mpsc::UnboundedSender<NotifyJob>>,
    /// Set on the writer task's own clone by [`Self::spawn_liveness_writer`]
    /// and nowhere else — the liveness row's `sensor_name`/`sensor_location`
    /// come off the live snapshot, so `/alarms` names an outage the way
    /// every other page names the device (review item 17, 2026-09-10).
    /// `None` on the shared instance and in a bare `open()` test, where the
    /// row falls back to the raw DevEUI/MAC as it did before.
    store: Option<Arc<crate::state::StateStore>>,
    /// The spill journal, for the one writer on this connection that has
    /// nobody to report a failure to: [`Self::write_liveness_transition`].
    /// Everything else here is request-scoped (ack, notes) and only retries.
    spill: Arc<SpillGate>,
    /// Last number [`Self::active_count`] returned, so the synchronous
    /// `routes::render` can draw the sidebar pill server-side instead of
    /// leaving an empty box for htmx to fill after the page has painted.
    /// See [`Self::cached_active_count`].
    badge_count: Arc<AtomicI64>,
    /// The notifier's live Hub fault verdicts, for the Hubs page — see
    /// [`crate::notify::HubFaults`]. Empty on a bare `open()`.
    hub_faults: crate::notify::HubFaults,
    /// Wakes the orphan sweeper ([`Self::spawn_orphan_sweeper`]) early, after
    /// a device, rule or Hub change ([`Self::request_orphan_sweep`]).
    orphan_poke: Arc<tokio::sync::Notify>,
}

impl AlarmService {
    /// Opens (creating if needed) `<data_dir>/portal.db` — the same path
    /// every other service struct uses — as this service's own connection.
    pub fn open(
        data_dir: &str,
        db_key: Option<&str>,
        spill: Arc<SpillGate>,
    ) -> Result<Self, StorageError> {
        std::fs::create_dir_all(data_dir)?;
        let path = Path::new(data_dir).join("portal.db");
        let storage = Storage::open(&path, db_key)?;
        let (nudge_tx, _rx) = broadcast::channel(NUDGE_CHANNEL_CAPACITY);
        Ok(Self {
            db: crate::db::Db::from_storage(storage, "alarms"),
            nudge_tx,
            relay: None,
            keys: None,
            notify_tx: None,
            store: None,
            spill,
            badge_count: Arc::new(AtomicI64::new(0)),
            hub_faults: Default::default(),
            orphan_poke: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Attaches the relay and key service the ack's `silence_buzzer` is
    /// signed and published through, and the notifier's job sender (ack's
    /// acknowledged-notification). Call once from `main` right after
    /// `open`.
    pub fn wire(
        mut self,
        relay: CommandRelay,
        keys: KeyService,
        notify_tx: mpsc::UnboundedSender<NotifyJob>,
    ) -> Self {
        self.relay = Some(relay);
        self.keys = Some(keys);
        self.notify_tx = Some(notify_tx);
        self
    }

    /// Shares the table the notifier publishes Hub faults into. Call once
    /// from `main`, next to [`Self::wire`].
    pub fn with_hub_faults(mut self, hub_faults: crate::notify::HubFaults) -> Self {
        self.hub_faults = hub_faults;
        self
    }

    pub fn hub_faults(&self) -> &crate::notify::HubFaults {
        &self.hub_faults
    }

    /// A bare `open()`-only instance for tests that don't exercise the ack
    /// mqtt/notify side effects (mirrors `CommandRelay`/`BackfillService`'s
    /// `test_instance`).
    #[cfg(test)]
    pub fn test_instance(data_dir: &str) -> Self {
        Self::open(data_dir, None, crate::durable::test_gate(data_dir)).expect("open alarm service")
    }

    /// rest.md `GET /api/alarms`.
    pub async fn list(
        &self,
        filter: AlarmListFilter,
    ) -> Result<(Vec<AlarmEvent>, i64), StorageError> {
        self.db.run(move |s| s.list_alarms(&filter)).await
    }

    /// rest.md `GET /api/alarms/{id}` — the alarm plus its notes, or `None`.
    pub async fn detail(
        &self,
        id: &str,
    ) -> Result<Option<(AlarmEvent, Vec<AlarmNote>)>, StorageError> {
        let id = id.to_string();
        self.db
            .run(move |s| {
                let Some(alarm) = s.get_alarm(&id)? else {
                    return Ok(None);
                };
                let notes = s.list_alarm_notes(&id)?;
                Ok(Some((alarm, notes)))
            })
            .await
    }

    /// The `/alarms` metric row's four numbers for one hostname, or every
    /// Hub when `None`, over the Hubs `known` accepts: an alarm of a Hub the
    /// registry does not know is not counted (decisions.md #30d, see
    /// [`hub_known`]).
    pub async fn counts(
        &self,
        hostname: Option<&str>,
        known: impl Fn(&str) -> bool,
    ) -> Result<AlarmCounts, StorageError> {
        let hostname = hostname.map(str::to_string);
        let per_hub = self
            .db
            .run(move |s| s.count_summary_by_hub(hostname.as_deref()))
            .await?;
        Ok(per_hub.into_iter().filter(|(hub, _)| known(hub)).fold(
            AlarmCounts::default(),
            |sum, (_, c)| AlarmCounts {
                active: sum.active + c.active,
                critical: sum.critical + c.critical,
                warning: sum.warning + c.warning,
                unacknowledged: sum.unacknowledged + c.unacknowledged,
            },
        ))
    }

    /// Active-alarm badge count for one hostname, or for every Hub when
    /// `None`, over the Hubs `known` accepts (see [`Self::counts`]). Also
    /// refreshes [`Self::cached_active_count`].
    pub async fn active_count(
        &self,
        hostname: Option<&str>,
        known: impl Fn(&str) -> bool,
    ) -> Result<i64, StorageError> {
        let count = self.counts(hostname, known).await?.active;
        self.badge_count.store(count, Ordering::Relaxed);
        Ok(count)
    }

    /// The last count [`Self::active_count`] read, without touching the
    /// database — `routes::render` is synchronous and cannot await one.
    ///
    /// The badge counts every Hub, so one number is the whole cache. It is 0
    /// until the first real count of the process, and it lags a mutation by
    /// the moment it takes the sidebar's own `/alarms/badge` fetch to land;
    /// that fetch is what corrects it, which is why the pill still carries
    /// its `load` trigger. The cache only decides what the box holds while
    /// the page paints, and being right about that on every navigation but
    /// the first beats an empty box on all of them.
    pub fn cached_active_count(&self) -> i64 {
        self.badge_count.load(Ordering::Relaxed)
    }

    /// rest.md `POST /alarms/{id}/acknowledge`: validates `reason` (non-blank,
    /// ≤2000 chars — `portal_domain::ack_validation`), stamps the ack,
    /// stores `reason` as a note (v1 parity: "reason becomes a note"),
    /// nudges the SSE alarm table, and sends `silence_buzzer` through the
    /// relay, signed with the acking user's key (design.md §5.4 — it was an
    /// unsigned bypass in v1).
    pub async fn acknowledge(
        &self,
        id: &str,
        user: &CurrentUser,
        reason: &str,
    ) -> Result<AckOutcome, AlarmError> {
        ack_validation(reason)?;

        let id_owned = id.to_string();
        let user_id = user.id;
        let username = user.username.clone();
        let reason_owned = reason.to_string();
        let now = iso_now();

        let outcome = self
            .db
            .run(move |s| {
                portal_storage::with_retry(|| {
                    let existing = s.get_alarm(&id_owned)?;
                    let was_already_acked = existing
                        .as_ref()
                        .is_some_and(|a| a.acknowledged_at.is_some());
                    let Some(updated) =
                        s.acknowledge_alarm(&id_owned, Some(user_id), &username, &now)?
                    else {
                        return Ok(None);
                    };
                    let note =
                        s.insert_alarm_note(&id_owned, Some(user_id), &username, &reason_owned)?;
                    Ok(Some((updated, note, was_already_acked)))
                })
            })
            .await?;

        let Some((alarm, note, was_already_acked)) = outcome else {
            return Err(AlarmError::NotFound);
        };

        let _ = self.nudge_tx.send(AlarmNudge {
            hostname: alarm.hostname.clone(),
            activated_ids: Vec::new(),
            escalated_ids: Vec::new(),
        });

        // An ended alarm is acknowledged to complete its trail (#28d): the
        // buzzer, if it sounds, sounds for another alarm of the Hub, and its
        // recipients already had the clear. An acknowledgement e-mail would
        // tell them the alarm "is still active", and a bulk ack of a page of
        // ended rows would send one per row. A second acknowledge silenced
        // nothing new (HS-12).
        let ended = alarm.cleared_at.is_some();
        if !ended && !was_already_acked {
            self.silence_buzzer(&alarm.hostname, user).await;
        }

        let should_notify = !was_already_acked && !ended;
        if should_notify {
            self.send_notify_job(NotifyJob::Acknowledged {
                alarm: alarm.clone(),
                user: user.username.clone(),
                reason: reason.to_string(),
            });
        }

        Ok(AckOutcome {
            alarm,
            note,
            should_notify,
        })
    }

    /// rest.md `POST /alarms/{id}/notes`. 404s (via [`AlarmError::NotFound`])
    /// when the alarm doesn't exist, per `insert_alarm_note`'s doc.
    pub async fn add_note(
        &self,
        id: &str,
        user: &CurrentUser,
        content: &str,
    ) -> Result<AlarmNote, AlarmError> {
        if content.trim().is_empty() {
            return Err(TextProblem::NoteBlank.into());
        }
        let id_owned = id.to_string();
        let user_id = user.id;
        let username = user.username.clone();
        let content_owned = content.to_string();

        let result = self
            .db
            .run(move |s| {
                portal_storage::with_retry(|| {
                    if s.get_alarm(&id_owned)?.is_none() {
                        return Ok(None);
                    }
                    let note =
                        s.insert_alarm_note(&id_owned, Some(user_id), &username, &content_owned)?;
                    Ok(Some(note))
                })
            })
            .await?;

        result.ok_or(AlarmError::NotFound)
    }

    pub fn subscribe_nudges(&self) -> broadcast::Receiver<AlarmNudge> {
        self.nudge_tx.subscribe()
    }

    /// The nudge sender, for the persist thread to broadcast on every
    /// engine-applied alarm mutation.
    pub fn nudge_sender(&self) -> broadcast::Sender<AlarmNudge> {
        self.nudge_tx.clone()
    }

    /// The ack's side effect, through the one chokepoint (design.md §7):
    /// signed with the acking user's key, so the Hub's `config_changes`
    /// row names them.
    ///
    /// **It can be refused, and the ack still stands.** `silence_buzzer` is
    /// `operate`, so a Viewer's certificate does not carry `level:operate`
    /// and the relay refuses before signing: the acknowledgement lands in
    /// the portal and the device keeps sounding. That is the behaviour
    /// design.md §13 item 6 asks product to confirm — if Viewers are who
    /// acks alarms in practice, the level of this verb is the thing to
    /// revisit. §16's UI-MQ2 is what surfaces it on the ack control, and is
    /// P6's.
    ///
    /// **`dispatch`, not `execute`: the ack must not wait for the Hub.**
    /// This was an unsigned fire-and-forget publish in v1 and nothing here
    /// consumes the device's answer, so waiting for `config/response` would
    /// only add `CommandRelay::COMMAND_TIMEOUT` to an HTTP handler — and
    /// `POST /alarms/bulk-acknowledge` walks the checked alarms one at a
    /// time, so a screenful of them against an offline Hub would outlive
    /// the reverse proxy and report a failure for acknowledgements that
    /// were all committed. The refusal above is still synchronous (it
    /// happens before anything is published); the device's own answer is
    /// logged by the relay's spawned half.
    async fn silence_buzzer(&self, hostname: &str, user: &CurrentUser) {
        let (Some(relay), Some(keys)) = (&self.relay, &self.keys) else {
            tracing::debug!(
                hostname,
                "alarm ack: no relay wired — skipping silence_buzzer"
            );
            return;
        };

        // `ensure_user_key` mints `silence_buzzer` + `level:operate` on a
        // manager's first ack — the union re-mint path every new permission
        // takes (design.md §4.3).
        let permissions = crate::perms::permissions_for(user);
        let key = match keys.ensure_user_key(user, hostname, &permissions).await {
            Ok(key) => key,
            Err(err) => {
                tracing::warn!(%err, hostname, "alarm ack: could not mint a signing key");
                return;
            }
        };

        let outcome = relay
            .dispatch(
                &user.username,
                &key,
                hostname,
                SILENCE_BUZZER,
                serde_json::json!({}),
                Some(format!("alarm acknowledged by {}", user.username)),
            )
            .await;
        if outcome.status != "sent" {
            tracing::warn!(
                hostname,
                user = %user.username,
                status = outcome.status,
                message = ?outcome.message,
                "alarm ack: silence_buzzer never reached the wire — the alarm is \
                 acknowledged in the portal, the device may still be sounding"
            );
        }
    }

    fn send_notify_job(&self, job: NotifyJob) {
        if let Some(tx) = &self.notify_tx {
            if tx.send(job).is_err() {
                tracing::warn!("notify channel closed — notification dropped");
            }
        }
    }

    // ---- MB3: liveness outages as alarm rows ---------------------------

    /// Turns the offline detector's per-device transitions into
    /// `alarm_events` rows (monitoring.backend.md §6 WP3.2, spec
    /// monitoring.md §6.3).
    ///
    /// Subscribes to the B3 seam (`offline::OfflineService::
    /// subscribe_transitions`) and consumes it forever — the loop is
    /// `BackfillService::run_transitions`' shape, down to treating a
    /// `Lagged` as "log and keep consuming": a missed edge is not
    /// recoverable by replay, and the next edge re-synchronises the row.
    ///
    /// Subscribe **before** `OfflineService::spawn_task`, or the first
    /// sweep — the one that classifies the whole fleet — happens before the
    /// receiver exists.
    ///
    /// **One row per outage** (decision 2026-09-08), which is what makes
    /// the window's list and the chart's marklines agree with the rest of
    /// the portal: `WentOffline` opens a row, `CameBack` stamps its
    /// `cleared_at`. A "back online" row of its own would have made "is
    /// this probe offline" a question about the newest row's type instead
    /// of the `cleared_at IS NULL` test every other alarm answers.
    pub fn spawn_liveness_writer(
        &self,
        mut transitions: broadcast::Receiver<crate::offline::HubTransition>,
        store: Arc<crate::state::StateStore>,
    ) {
        let mut service = self.clone();
        service.store = Some(store);
        tokio::spawn(async move {
            loop {
                match transitions.recv().await {
                    Ok(t) => service.write_liveness_transition(t).await,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            missed = n,
                            "liveness writer missed offline transitions — the next edge re-syncs"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    /// Asks the orphan sweeper for a pass soon — after a device removal or a
    /// rule change, so the closed row does not wait for the next period.
    pub fn request_orphan_sweep(&self) {
        self.orphan_poke.notify_one();
    }

    /// Closes the open alarms of a Node or Beacon just removed from its Hub
    /// (`device_removed`, audited). Best-effort: logged, never the caller's
    /// failure, like the other removal clean-ups.
    pub async fn close_removed_device(&self, hostname: &str, target: AlarmTarget) {
        let host = hostname.to_string();
        let closed = self
            .db
            .run(move |s| orphans::close_device_alarms(s, &host, &target, &iso_now()))
            .await;
        match closed {
            Ok(0) => {}
            Ok(n) => {
                tracing::info!(
                    hostname,
                    closed = n,
                    "device removed; its open alarms closed"
                );
                let _ = self.nudge_tx.send(AlarmNudge {
                    hostname: hostname.to_string(),
                    activated_ids: Vec::new(),
                    escalated_ids: Vec::new(),
                });
            }
            Err(err) => tracing::warn!(%err, hostname, "device removed but its alarms stay open"),
        }
    }

    /// The orphan sweep (plan/portal/monitoring.md, amendment 2026-10-14):
    /// a first pass once the registry has had `grace` to fill from retained
    /// messages after the start, then every `period` and on every
    /// [`Self::request_orphan_sweep`]. See [`orphans`].
    pub fn spawn_orphan_sweeper(
        &self,
        store: Arc<crate::state::StateStore>,
        grace: std::time::Duration,
        period: std::time::Duration,
    ) {
        let service = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            loop {
                service.sweep_orphans_once(&store).await;
                tokio::select! {
                    _ = tokio::time::sleep(period) => {}
                    _ = service.orphan_poke.notified() => {
                        // Let the change that poked settle (the Hub's echo).
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }
            }
        });
    }

    /// One sweep pass; logs and nudges, never fails.
    pub async fn sweep_orphans_once(&self, store: &crate::state::StateStore) {
        let snapshot = store.snapshot();
        let report = self
            .db
            .run(move |s| orphans::sweep(s, &snapshot, &iso_now()))
            .await;
        match report {
            Ok(r) if r.closed.is_empty() && r.rekeyed.is_empty() => {}
            Ok(r) => {
                tracing::info!(
                    closed = r.closed.len(),
                    rekeyed = r.rekeyed.len(),
                    "orphan sweep: open alarms without a device, Hub or rule"
                );
                for hostname in r.hosts {
                    let _ = self.nudge_tx.send(AlarmNudge {
                        hostname,
                        activated_ids: Vec::new(),
                        escalated_ids: Vec::new(),
                    });
                }
            }
            Err(err) => tracing::warn!(%err, "orphan sweep failed; the next pass retries"),
        }
    }

    /// One edge. Idempotent in both directions through `active_alarm`: a
    /// second `WentOffline` with a row already open writes nothing (a
    /// process restart re-arms the detector and can re-announce an outage
    /// that never ended), and a `CameBack` with nothing open writes nothing
    /// either (the portal started while the device was already silent, so
    /// there is no outage of ours to close).
    async fn write_liveness_transition(&self, t: crate::offline::HubTransition) {
        use crate::notify::offline::Transition;

        let hostname = t.hostname.clone();
        let sensor_id = liveness_sensor_id(t.transition.scope, &t.transition.entity);
        let opening = t.transition.transition == Transition::WentOffline;
        let message = crate::offline::offline_message(&t.transition);
        let now = iso_now();

        let entity = t.transition.entity.clone();
        // What the rest of the portal calls this device. `/alarms` renders
        // `sensor_name` and `sensor_location` straight out of the row, so a
        // liveness row that carried only the raw DevEUI/MAC read as a
        // different device from the one on `/nodes` and `/beacons` (review
        // item 17). Falls back to the identity when the snapshot has not
        // heard of it — a device can go offline and be forgotten.
        let (display_name, display_location) = self
            .store
            .as_ref()
            .and_then(|s| device_display(&s.snapshot(), &hostname, t.transition.scope, &entity))
            .unwrap_or((entity.clone(), None));
        let edge = crate::durable::LivenessEdge {
            hostname: hostname.clone(),
            sensor_id: sensor_id.clone(),
            opening,
            message,
            at: now,
            display_name,
            display_location,
        };

        // Spilled rather than retried: this edge has no second announcement.
        // `OfflineService::sweep` broadcasts one message per device that
        // actually *changed* verdict, so a write lost to a locked database is
        // lost for the life of the process — and a lost `CameBack` leaves the
        // device reading offline forever, because `/alarms`, the probe window
        // and the chart marklines all answer that question with
        // `cleared_at IS NULL`.
        let spill = Arc::clone(&self.spill);
        let for_db = edge.clone();
        let written = self
            .db
            .run(move |s| crate::durable::write_liveness(&spill, s, &for_db))
            .await;

        match written {
            Ok(crate::durable::LivenessOutcome::Wrote) => {}
            Ok(crate::durable::LivenessOutcome::Redundant) => {
                tracing::debug!(
                    hostname,
                    sensor_id,
                    opening,
                    "liveness edge needed no alarm row"
                );
                return;
            }
            // On disk, not applied yet. `persist::drain_spill` replays it and
            // sends the nudge then, so returning here delays the SSE re-render
            // rather than dropping it (sqlite-vacuum.md §3.3).
            Ok(crate::durable::LivenessOutcome::Spilled) => {
                tracing::info!(
                    hostname,
                    sensor_id,
                    opening,
                    "liveness edge journalled; it lands when the rewrite finishes"
                );
                return;
            }
            // Both the database and the journal refused it. `write_or_spill`
            // has already logged the detail at `error!`.
            Err(err) => {
                tracing::error!(%err, hostname, sensor_id, "liveness edge could not be persisted or spilled");
                return;
            }
        }

        // `activated_ids` empty on purpose: the row belongs in the alarm
        // table and in the probe window, but a toast for it would duplicate
        // the notifier's own offline email and the fault triangle the row
        // already grew (§15.4).
        let _ = self.nudge_tx.send(AlarmNudge {
            hostname,
            activated_ids: Vec::new(),
            escalated_ids: Vec::new(),
        });
    }
}

/// The name and location the rest of the portal shows for one device, out
/// of the live snapshot.
///
/// `None` when the snapshot has no such device — the caller then falls back
/// to the raw identity, which is still better than an empty cell. The keys
/// are the snapshot's own (lowercase `node_key`, uppercase MAC), which is
/// exactly what the detector's `entity` already is: `OfflineService::sweep`
/// builds one `DeviceLiveness` per map entry.
fn device_display(
    snapshot: &crate::state::PortalState,
    hostname: &str,
    scope: crate::notify::offline::DeviceScope,
    entity: &str,
) -> Option<(String, Option<String>)> {
    use crate::notify::offline::DeviceScope;
    let device = snapshot.device(hostname)?;
    match scope {
        DeviceScope::Node => {
            let n = device.nodes.get(entity)?;
            Some((
                n.name.clone().unwrap_or_else(|| entity.to_string()),
                n.location.clone(),
            ))
        }
        DeviceScope::Beacon => {
            let b = device.ble_sensors.get(entity)?;
            Some((
                b.name.clone().unwrap_or_else(|| entity.to_string()),
                b.location.clone(),
            ))
        }
    }
}

/// The `alarm_events.sensor_id` a liveness row carries.
///
/// Third component `liveness` (decision 2026-09-08), reserved: the value
/// classes put a **field label** there (`lrw:{key}:Temperature`,
/// `eye:{mac}:Humidity`) and the native slots put `native:{slot}`, so
/// `liveness` is a name no field can take — the registry has no field
/// called that, and one would now be a collision rather than a surprise.
/// The probe window fetches all three with one `sensor_id_prefix` query.
///
/// Wired probes get no such id by construction: they have no per-probe
/// liveness, only the Hub's `DeviceOffline` (monitoring.backend.md §9.2),
/// and the window says so instead of showing an empty section.
fn liveness_sensor_id(scope: crate::notify::offline::DeviceScope, entity: &str) -> String {
    match scope {
        crate::notify::offline::DeviceScope::Node => {
            format!("{}liveness", portal_domain::node_sensor_prefix(entity))
        }
        crate::notify::offline::DeviceScope::Beacon => format!("eye:{entity}:liveness"),
    }
}

/// RFC3339 UTC "now" — same convention `alarm_engine`/`portal_storage`
/// timestamps use elsewhere in this binary.
fn iso_now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use portal_storage::NewAlarmEvent;

    #[test]
    fn alarm_field_reads_the_sensor_id_convention() {
        let f = |id, name| alarm_field(id, name).map(|f| (f.name, f.unit));
        assert_eq!(f("line-5", None), Some(("temperature", Some("\u{00b0}C"))));
        assert_eq!(
            f("lrw:70b3d5:Humidity", Some("Humidity")),
            Some(("humidity", Some("%")))
        );
        assert_eq!(
            f("p2p:70b3d5:Machine probe 1 \u{2014} Hum", None),
            Some(("machine_probe_humidity_1", Some("%")))
        );
        assert_eq!(
            f("lrw:70b3d5:native:0", Some("temperature")),
            Some(("temperature", Some("\u{00b0}C")))
        );
        assert_eq!(
            f("lrw:70b3d5:native:1", Some("Node \u{b7} Humidity")),
            Some(("humidity", Some("%"))),
            "a v2 Node's own alarm (N23)"
        );
        assert_eq!(
            f("eye:C0FFEE:Humidity", Some("Humidity")),
            Some(("humidity", Some("%")))
        );
        assert_eq!(
            f("eye:AA:BB:CC:DD:EE:FF:Humidity", Some("Humidity")),
            Some(("humidity", Some("%"))),
            "a real MAC has colons of its own"
        );
        assert_eq!(
            f("lrw:70b3d5:liveness", None),
            None,
            "a device alarm has no quantity"
        );
        assert_eq!(f("sys:wifi", None), None);
    }

    fn new_user(id: i64, username: &str) -> CurrentUser {
        CurrentUser {
            id,
            username: username.to_string(),
            full_name: None,
            user_role: crate::auth::Role::Viewer,
            is_superuser: false,
            monitoring_view_mode: "table-simplified".into(),
        }
    }

    fn seed_alarm(svc: &AlarmService) -> String {
        seed_alarm_on(svc, "h1")
    }

    fn seed_alarm_on(svc: &AlarmService, hostname: &str) -> String {
        // Direct sync insert against the same on-disk db, bypassing the
        // async wrapper for test setup brevity.
        let guard = svc.db.lock().unwrap();
        guard
            .insert_alarm_event(&NewAlarmEvent {
                hostname: hostname.into(),
                sensor_id: "line-0".into(),
                sensor_name: Some("Fridge".into()),
                sensor_location: None,
                alarm_type: "HIGH".into(),
                severity: "warning".into(),
                from_state: Some("NORMAL".into()),
                to_state: Some("WARNING".into()),
                message: None,
                value: Some(9.0),
                threshold: Some(8.0),
                triggered_at: "2026-08-18T00:00:00Z".into(),
                cleared_at: None,
                acknowledged_at: None,
                acknowledged_by_user_id: None,
                acknowledged_by_username: None,
                native_key: None,
            })
            .expect("seed alarm")
    }

    #[tokio::test]
    async fn ack_requires_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let svc = AlarmService::open(
            dir.path().to_str().unwrap(),
            None,
            crate::durable::test_gate(dir.path().to_str().unwrap()),
        )
        .unwrap();
        let id = seed_alarm(&svc);
        let user = new_user(1, "alice");

        let err = svc.acknowledge(&id, &user, "   ").await.unwrap_err();
        assert!(matches!(err, AlarmError::InvalidText(_)));
    }

    #[tokio::test]
    async fn ack_unknown_id_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let svc = AlarmService::open(
            dir.path().to_str().unwrap(),
            None,
            crate::durable::test_gate(dir.path().to_str().unwrap()),
        )
        .unwrap();
        let user = new_user(1, "alice");

        let err = svc
            .acknowledge("no-such-id", &user, "checked it")
            .await
            .unwrap_err();
        assert!(matches!(err, AlarmError::NotFound));
    }

    #[tokio::test]
    async fn ack_stores_reason_as_note_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let svc = AlarmService::open(
            dir.path().to_str().unwrap(),
            None,
            crate::durable::test_gate(dir.path().to_str().unwrap()),
        )
        .unwrap();
        let id = seed_alarm(&svc);
        let user = new_user(1, "alice");

        let first = svc
            .acknowledge(&id, &user, "confirmed on-site")
            .await
            .unwrap();
        assert!(first.alarm.acknowledged_at.is_some());
        assert_eq!(first.note.content, "confirmed on-site");
        assert!(first.should_notify, "first ack should notify");

        let (_alarm, notes) = svc.detail(&id).await.unwrap().unwrap();
        assert_eq!(notes.len(), 1);

        // Re-ack: overwrites the stamp (idempotent) but must not re-fire the
        // acknowledged-notification.
        let second = svc
            .acknowledge(&id, &user, "still confirmed")
            .await
            .unwrap();
        assert!(!second.should_notify, "re-ack must not re-notify");

        let (_alarm, notes) = svc.detail(&id).await.unwrap().unwrap();
        assert_eq!(notes.len(), 2, "re-ack still records its own note");
    }

    /// MR !103 review: an ended alarm is acknowledged for its trail only
    /// (#28d). Its recipients had the clear; an acknowledgement saying the
    /// alarm "is still active" would be wrong, once per row of a bulk ack.
    #[tokio::test]
    async fn acking_an_ended_alarm_notifies_nobody() {
        let dir = tempfile::tempdir().unwrap();
        let svc = AlarmService::open(
            dir.path().to_str().unwrap(),
            None,
            crate::durable::test_gate(dir.path().to_str().unwrap()),
        )
        .unwrap();
        let id = seed_alarm(&svc);
        svc.db
            .lock()
            .unwrap()
            .clear_alarm(&id, "2026-08-18T01:00:00Z", None)
            .unwrap();

        let outcome = svc
            .acknowledge(&id, &new_user(1, "alice"), "trail completed")
            .await
            .unwrap();
        assert!(outcome.alarm.acknowledged_at.is_some(), "the ack stands");
        assert!(!outcome.should_notify, "but nobody hears of it");
    }

    #[tokio::test]
    async fn add_note_requires_existing_alarm_and_nonblank_content() {
        let dir = tempfile::tempdir().unwrap();
        let svc = AlarmService::open(
            dir.path().to_str().unwrap(),
            None,
            crate::durable::test_gate(dir.path().to_str().unwrap()),
        )
        .unwrap();
        let id = seed_alarm(&svc);
        let user = new_user(2, "bob");

        let err = svc.add_note("missing", &user, "hi").await.unwrap_err();
        assert!(matches!(err, AlarmError::NotFound));

        let err = svc.add_note(&id, &user, "   ").await.unwrap_err();
        assert!(matches!(err, AlarmError::InvalidText(_)));

        let note = svc.add_note(&id, &user, "looks fine now").await.unwrap();
        assert_eq!(note.username, "bob");
    }

    #[tokio::test]
    async fn active_count_and_list_reflect_seeded_alarm() {
        let dir = tempfile::tempdir().unwrap();
        let svc = AlarmService::open(
            dir.path().to_str().unwrap(),
            None,
            crate::durable::test_gate(dir.path().to_str().unwrap()),
        )
        .unwrap();
        seed_alarm(&svc);

        assert_eq!(svc.active_count(Some("h1"), |_| true).await.unwrap(), 1);
        let (rows, total) = svc.list(AlarmListFilter::default()).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows.len(), 1);
    }

    /// devices.hub.md §12 E: without a hostname the badge and the metric row
    /// count every Hub, not the first one in the store.
    #[tokio::test]
    async fn an_unfiltered_count_spans_every_hub() {
        let dir = tempfile::tempdir().unwrap();
        let svc = AlarmService::open(
            dir.path().to_str().unwrap(),
            None,
            crate::durable::test_gate(dir.path().to_str().unwrap()),
        )
        .unwrap();
        seed_alarm_on(&svc, "h1");
        seed_alarm_on(&svc, "h2");

        assert_eq!(svc.active_count(Some("h1"), |_| true).await.unwrap(), 1);
        assert_eq!(svc.active_count(None, |_| true).await.unwrap(), 2);
        // decisions.md #30d: a Hub the registry does not know is not counted.
        assert_eq!(svc.active_count(None, |h| h == "h1").await.unwrap(), 1);
        let counts = svc.counts(None, |_| true).await.unwrap();
        assert_eq!(
            (counts.active, counts.warning, counts.unacknowledged),
            (2, 2, 2)
        );
        assert_eq!(svc.counts(Some("h2"), |_| true).await.unwrap().active, 1);
    }

    // ---- MB3: liveness outages as alarm rows -------------------------

    mod liveness {
        use super::*;
        use crate::notify::offline::{DeviceScope, OfflineTransition, Transition};
        use crate::offline::HubTransition;

        const HOST: &str = "h1";

        fn edge(scope: DeviceScope, entity: &str, transition: Transition) -> HubTransition {
            HubTransition {
                hostname: HOST.into(),
                transition: OfflineTransition {
                    scope,
                    entity: entity.into(),
                    transition,
                    silent_for_s: 3_600,
                },
            }
        }

        /// Every row this service holds, cleared ones included — the
        /// window's own read is `include_cleared: true`, because a closed
        /// outage is exactly what the timeline is for.
        async fn rows(svc: &AlarmService) -> Vec<AlarmEvent> {
            svc.list(AlarmListFilter {
                limit: 100,
                include_cleared: true,
                ..Default::default()
            })
            .await
            .expect("list")
            .0
        }

        /// The §6.3 shape, in full: the onset opens exactly one row with
        /// the agreed spelling, and the recovery closes **that** row rather
        /// than opening a second one (decision 2026-09-08).
        #[tokio::test]
        async fn an_offline_transition_opens_one_alarm_row_and_a_recovery_clears_it() {
            let dir = tempfile::tempdir().expect("tempdir");
            let svc = AlarmService::test_instance(dir.path().to_str().expect("utf8"));

            svc.write_liveness_transition(edge(DeviceScope::Node, "aa11", Transition::WentOffline))
                .await;

            let open = rows(&svc).await;
            assert_eq!(open.len(), 1);
            let a = &open[0];
            assert_eq!(a.alarm_type, "OFFLINE");
            assert_eq!(a.severity, "warning");
            assert_eq!(a.sensor_id, "lrw:aa11:liveness");
            assert_eq!(a.from_state.as_deref(), Some("NORMAL"));
            assert_eq!(a.to_state.as_deref(), Some("DISCONNECTED"));
            assert!(a.cleared_at.is_none());
            // Nothing is measured here, only silence.
            assert_eq!((a.value, a.threshold), (None, None));
            assert!(a.native_key.is_none());
            assert!(
                a.message
                    .as_deref()
                    .is_some_and(|m| m.contains("60 minutes")),
                "the row carries the notifier's own sentence: {:?}",
                a.message
            );
            assert_eq!(
                svc.active_count(Some(HOST), |_| true).await.expect("count"),
                1
            );

            svc.write_liveness_transition(edge(DeviceScope::Node, "aa11", Transition::CameBack))
                .await;

            let after = rows(&svc).await;
            assert_eq!(after.len(), 1, "the recovery is a clear, not a second row");
            assert!(after[0].cleared_at.is_some());
            assert_eq!(
                svc.active_count(Some(HOST), |_| true).await.expect("count"),
                0
            );
        }

        /// A restart re-arms the detector, so the very next sweep can
        /// re-announce an outage that never ended. `active_alarm` is what
        /// makes that a no-op instead of a duplicate row per restart.
        #[tokio::test]
        async fn a_repeated_offline_edge_does_not_open_a_second_row() {
            let dir = tempfile::tempdir().expect("tempdir");
            let svc = AlarmService::test_instance(dir.path().to_str().expect("utf8"));

            for _ in 0..3 {
                svc.write_liveness_transition(edge(
                    DeviceScope::Node,
                    "aa11",
                    Transition::WentOffline,
                ))
                .await;
            }
            assert_eq!(rows(&svc).await.len(), 1);

            // And after a recovery, a fresh outage IS a new row — the
            // idempotence is per open outage, not per device forever.
            svc.write_liveness_transition(edge(DeviceScope::Node, "aa11", Transition::CameBack))
                .await;
            svc.write_liveness_transition(edge(DeviceScope::Node, "aa11", Transition::WentOffline))
                .await;
            let all = rows(&svc).await;
            assert_eq!(all.len(), 2);
            assert_eq!(
                all.iter().filter(|a| a.cleared_at.is_none()).count(),
                1,
                "exactly one outage is open"
            );
        }

        /// The portal can start while a device is already silent, so the
        /// first edge it ever sees for that device may be the recovery.
        /// There is no outage of ours to close, and inventing one would put
        /// a zero-length outage in the timeline that never happened.
        #[tokio::test]
        async fn a_recovery_with_no_open_outage_writes_nothing() {
            let dir = tempfile::tempdir().expect("tempdir");
            let svc = AlarmService::test_instance(dir.path().to_str().expect("utf8"));

            svc.write_liveness_transition(edge(DeviceScope::Beacon, "AA:BB", Transition::CameBack))
                .await;
            assert!(rows(&svc).await.is_empty());
        }

        /// A Node and a Beacon can carry the same entity string — nothing
        /// stops an operator naming a Node `AA:BB:CC:DD:EE:01` — so the
        /// scope has to be in the key, not only in the caller's head.
        #[tokio::test]
        async fn a_beacon_and_a_node_with_the_same_entity_string_do_not_collide() {
            let dir = tempfile::tempdir().expect("tempdir");
            let svc = AlarmService::test_instance(dir.path().to_str().expect("utf8"));
            let entity = "AA:BB:CC:DD:EE:01";

            svc.write_liveness_transition(edge(DeviceScope::Node, entity, Transition::WentOffline))
                .await;
            svc.write_liveness_transition(edge(
                DeviceScope::Beacon,
                entity,
                Transition::WentOffline,
            ))
            .await;

            let mut ids: Vec<String> = rows(&svc).await.into_iter().map(|a| a.sensor_id).collect();
            ids.sort();
            assert_eq!(
                ids,
                vec![
                    format!("eye:{entity}:liveness"),
                    format!("lrw:{entity}:liveness"),
                ]
            );

            // Clearing one leaves the other open.
            svc.write_liveness_transition(edge(DeviceScope::Node, entity, Transition::CameBack))
                .await;
            let open: Vec<String> = rows(&svc)
                .await
                .into_iter()
                .filter(|a| a.cleared_at.is_none())
                .map(|a| a.sensor_id)
                .collect();
            assert_eq!(open, vec![format!("eye:{entity}:liveness")]);
        }

        /// End-to-end over the seam MB3 consumes rather than defines: a
        /// real `OfflineService` sweep, a real `broadcast`, and the task
        /// `spawn_liveness_writer` actually spawns. Every test above calls
        /// `write_liveness_transition` directly and would pass with the
        /// writer never spawned at all — which is precisely the wiring
        /// `main.rs`'s ordering comment is about.
        ///
        /// Multi-threaded and real-time for the reason `backfill.rs`'s
        /// equivalent gives: the path crosses `spawn_blocking` twice and
        /// those are not ordered by `tokio::time::advance`.
        #[tokio::test(flavor = "multi_thread")]
        async fn the_liveness_writer_receives_from_the_real_broadcast() {
            use crate::state::events::{IngestEvent, NodeReadingMsg};
            use crate::state::StateStore;

            // 2026-01-13T14:30:00Z, the timestamp `offline.rs`'s tests pin.
            const SEEN_AT: i64 = 1_768_314_600;

            let dir = tempfile::tempdir().expect("tempdir");
            let data_dir = dir.path().to_str().expect("utf8");
            let svc = AlarmService::test_instance(data_dir);

            let store = std::sync::Arc::new(StateStore::new());
            store.apply(IngestEvent::NodeReading {
                hostname: HOST.into(),
                node_key: "aa11".into(),
                payload: Box::new(NodeReadingMsg {
                    message_id: "m-1".into(),
                    dev_eui: "aa11".into(),
                    serial: None,
                    fields: [("temperature".to_string(), 4.2)].into_iter().collect(),
                    counters: Default::default(),
                    events: Vec::new(),
                    gateways: Vec::new(),
                    rssi: None,
                    snr: None,
                    fport: None,
                    page_index: None,
                    page_count: None,
                    dr: None,
                    received_at: "2026-01-13T14:30:00Z".to_string(),
                }),
            });

            let db = crate::db::Db::open(data_dir, None, "test").expect("open db");
            let offline = crate::offline::OfflineService::test_instance(db);

            // The ordering `main.rs` documents: subscribe BEFORE the sweep
            // can run. Reversed, this test is what fails.
            svc.spawn_liveness_writer(
                offline.subscribe_transitions(),
                std::sync::Arc::clone(&store),
            );

            // No config for this Node, so the detector's 900 s fallback and
            // the default K = 4 give a 3 600 s budget.
            let went = offline.pass(&store, SEEN_AT + 3_601).await;
            assert_eq!(went.len(), 1, "the sweep must produce the onset");

            // The spawned task is on another thread; poll rather than sleep
            // a fixed amount.
            let mut open = Vec::new();
            for _ in 0..200 {
                open = rows(&svc).await;
                if !open.is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            assert_eq!(open.len(), 1, "the onset must reach the spawned writer");
            assert_eq!(open[0].sensor_id, "lrw:aa11:liveness");
            assert_eq!(open[0].alarm_type, "OFFLINE");
            // Review item 17: `/alarms` reads `sensor_name`/`sensor_location`
            // straight out of the row, so the writer takes them off the
            // snapshot rather than leaving the raw DevEUI there. This Node
            // reported no name, so the fallback is the identity — and the
            // location column is the snapshot's, not a hard-coded `None`.
            assert_eq!(open[0].sensor_name.as_deref(), Some("aa11"));
        }
    }
}

/// The ack side effect, end to end (plan/portal/mqtt/design.md §5.4).
///
/// Kept in its own module because it needs the pieces a bare
/// `AlarmService::test_instance` deliberately does not have: a relay, a key
/// store, and a paired hostname to mint a certificate under.
#[cfg(test)]
mod ack_side_effect_tests {
    use super::*;
    use crate::relay::Publisher;
    use futures_util::future::BoxFuture;
    use portal_storage::NewAlarmEvent;
    use serde_json::Value;
    use std::sync::Mutex as TestMutex;

    type Captured = Arc<TestMutex<Vec<(String, Vec<u8>)>>>;

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

    const HOST: &str = "FIBER-001";

    fn user(id: i64, username: &str, role: crate::auth::Role) -> CurrentUser {
        CurrentUser {
            id,
            username: username.to_string(),
            full_name: None,
            user_role: role,
            is_superuser: false,
            monitoring_view_mode: "table-simplified".into(),
        }
    }

    async fn service() -> (AlarmService, Captured, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8 path");

        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let relay = crate::relay::CommandRelay::new(publisher, "proximos");

        let keys = KeyService::open(data_dir, None).expect("open keys");
        keys.store_pairing(
            HOST,
            Some("test-ca".into()),
            "00".repeat(32),
            [7u8; 32],
            None,
            None,
        )
        .await
        .expect("pair the hub");
        // `user_device_keys.user_id` is a foreign key into `users`, so the
        // acking user has to exist before a key can be minted for them.
        // Seeded through a second connection on the same WAL db, the way
        // `backfill.rs`'s tests do.
        portal_storage::Storage::open(&dir.path().join("portal.db"), None)
            .expect("side door")
            .create_user(&portal_storage::NewUser {
                username: "manager1".into(),
                password_hash: "x".into(),
                full_name: None,
                job_title: None,
                user_role: "manager".into(),
                is_superuser: false,
            })
            .expect("seed user");

        let (notify_tx, _rx) = mpsc::unbounded_channel();
        let service = AlarmService::open(data_dir, None, crate::durable::test_gate(data_dir))
            .expect("open alarm service")
            .wire(relay, keys, notify_tx);
        (service, sent, dir)
    }

    fn seed_alarm(svc: &AlarmService) -> String {
        let guard = svc.db.lock().unwrap();
        guard
            .insert_alarm_event(&NewAlarmEvent {
                hostname: HOST.into(),
                sensor_id: "line-0".into(),
                sensor_name: Some("Fridge".into()),
                sensor_location: None,
                alarm_type: "HIGH".into(),
                severity: "warning".into(),
                from_state: Some("NORMAL".into()),
                to_state: Some("WARNING".into()),
                message: None,
                value: Some(9.0),
                threshold: Some(8.0),
                triggered_at: "2026-08-18T00:00:00Z".into(),
                cleared_at: None,
                acknowledged_at: None,
                acknowledged_by_user_id: None,
                acknowledged_by_username: None,
                native_key: None,
            })
            .expect("seed alarm")
    }

    /// A manager's ack silences the buzzer with a **signed** `operate`
    /// command carrying that manager's own signer id — which is what puts
    /// the acking user on the Hub's audit trail. v1 published
    /// `{"command": "silence_buzzer"}` unsigned, with no permission mapping
    /// at all, and recorded nobody.
    #[tokio::test]
    async fn a_managers_ack_signs_silence_buzzer_as_that_manager() {
        let (service, sent, _dir) = service().await;
        let id = seed_alarm(&service);
        let manager = user(1, "manager1", crate::auth::Role::Manager);

        // Nothing answers, and the ack does not care: `dispatch` returns
        // once the frame is published, so this is a plain await with no
        // timeout to escape.
        service
            .acknowledge(&id, &manager, "checked the fridge")
            .await
            .expect("the acknowledgement succeeds");

        let captured = sent.lock().unwrap().clone();
        assert_eq!(captured.len(), 1, "one command, not a flat-dialect publish");
        assert_eq!(captured[0].0, "proximos/fiber-001/commands/config/request");

        let body: Value = serde_json::from_slice(&captured[0].1).expect("an envelope");
        assert_eq!(body["command_type"], "silence_buzzer");
        assert_eq!(body["level"], "operate");
        assert_eq!(body["v"], 2);
        assert_eq!(body["signer_id"], "user-1-manager1");
        assert!(
            body["reason"]
                .as_str()
                .is_some_and(|r| r.contains("manager1")),
            "the reason names the acking user: {body}"
        );
        let permissions = body["certificate"]["permissions"]
            .as_array()
            .expect("permissions");
        assert!(permissions.iter().any(|p| p == "silence_buzzer"));
        assert!(
            permissions.iter().any(|p| p == "level:operate"),
            "the union re-mint adds the level string on the first ack"
        );
    }

    /// design.md §13 item 6, pinned rather than argued: `silence_buzzer` is
    /// `operate`, a Viewer's ceiling is `read`, so a Viewer's ack lands in
    /// the portal and the device keeps sounding. If Viewers are who acks
    /// alarms in practice, this is the row to revisit — the test exists so
    /// that conversation starts from the behaviour, not from a guess.
    /// **The ack does not wait for the Hub**, which is what makes
    /// `POST /alarms/bulk-acknowledge` usable: that route walks the checked
    /// alarms one at a time (`routes/alarm_routes.rs`), so if each ack
    /// blocked on `config/response` a screenful of them against an offline
    /// Hub would hold one HTTP request for `alarms × COMMAND_TIMEOUT` —
    /// past any reverse proxy, reporting a failure for acknowledgements
    /// that had every one of them committed.
    ///
    /// Nothing answers these publishes, so the budget below is the
    /// assertion: three acks settle in milliseconds, where three waits
    /// would need a minute of real time.
    #[tokio::test]
    async fn acking_several_alarms_does_not_wait_for_the_device() {
        let (service, sent, _dir) = service().await;
        let manager = user(1, "manager1", crate::auth::Role::Manager);
        let ids: Vec<String> = (0..3).map(|_| seed_alarm(&service)).collect();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            for id in &ids {
                service
                    .acknowledge(id, &manager, "checked the fridge")
                    .await
                    .expect("the acknowledgement succeeds");
            }
        })
        .await
        .expect("a bulk ack must not wait out the relay's command timeout");

        assert_eq!(
            sent.lock().unwrap().len(),
            3,
            "one silence_buzzer per ack, all of them on the wire"
        );
    }

    #[tokio::test]
    async fn a_viewers_ack_stands_but_does_not_silence_the_device() {
        let (service, sent, _dir) = service().await;
        let id = seed_alarm(&service);
        let viewer = user(1, "manager1", crate::auth::Role::Viewer);

        let outcome = service
            .acknowledge(&id, &viewer, "noted")
            .await
            .expect("the acknowledgement itself succeeds");
        assert!(outcome.alarm.acknowledged_at.is_some());
        assert!(
            sent.lock().unwrap().is_empty(),
            "refused before signing — nothing reached the wire"
        );
    }
}
