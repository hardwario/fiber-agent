//! The notifier task (M6-E): consumes [`NotifyJob`]s pushed by the persist
//! thread (one per `AlarmAction::Notify` it applies) and by
//! `AlarmService::acknowledge` (a fresh, non-repeat acknowledgement), then
//! drives the M6-N transports (`notify::email`/`notify::webhook`) and
//! `report_history` bookkeeping. System alerts (battery/storage/network/
//! offline/version) are evaluated here too, holding one `SystemAlertState`
//! per hostname — in-memory, reset on restart (v1 parity, see
//! `notify::checkers`'s module doc).
//!
//! # Storage
//!
//! Owns its own `Storage` connection (SQLite connection #7, after persist /
//! auth / read-db / keys / backfill / alarms) rather than sharing
//! `AlarmService`'s — a documented deviation from "AlarmService's storage
//! (or its own — your call)" in the work order: keeping this fully
//! decoupled means a slow/hanging email or webhook send never contends the
//! mutex a browser-facing `AlarmService::list` request needs.
//!
//! Never crashes: every load/send/write failure is logged and, where
//! meaningful, recorded as a `failed` `report_history` row instead of
//! propagating.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::mpsc;

use portal_storage::{
    AlarmEvent, DeviceReportConfig, NewReportHistory, NotifyGlobalConfig, SpillGate, Storage,
    StorageError, HISTORY_SOURCE_REPORT, HISTORY_SOURCE_SYSTEM_ALERT, HISTORY_STATUS_FAILED,
    HISTORY_STATUS_SENT,
};

use crate::notify::checkers::{
    SystemAlertConfig as CheckerSystemAlertConfig, SystemAlertState, SystemSnapshot,
};
use crate::notify::email::EmailTransport;
use crate::notify::payload::{
    AckInfo, AlarmNotification, AlarmSource as NotifyAlarmSource, NotifyGates,
    NotifyKind as EmailNotifyKind,
};
use crate::notify::seal::NotifySealer;
use crate::notify::webhook::{send_webhook, WebhookConfig};

/// One notify-worthy sensor's flattened facts — built by the persist thread
/// from the alarm engine's `NotifyPayload` (`Activated`) or the freshly
/// cleared/updated stored row (`Cleared`, which needs the row's `threshold`
/// per `alarm_engine`'s own doc: the engine's `ActiveAlarm` doesn't carry
/// one). Kept free of engine/storage types so this module only depends on
/// `notify::`'s own vocabulary.
#[derive(Debug, Clone)]
pub struct AlarmNotifyData {
    pub alarm_id: String,
    pub hostname: String,
    /// `"warning"` | `"critical"`.
    pub severity: String,
    /// `"HIGH"` | `"LOW"` | `"DISCONNECT"` | `"POWER_DISCONNECT"` | … —
    /// `portal_storage::AlarmEvent::alarm_type`'s canonical spelling.
    pub alarm_type: String,
    pub source: NotifyAlarmSource,
    pub value: Option<f64>,
    pub threshold: Option<f64>,
    pub message: Option<String>,
    pub sensor_name: String,
    pub sensor_id: String,
    pub line: Option<i32>,
    pub location: Option<String>,
    /// The Node's `dev_eui`, on both transports (GitLab #73).
    pub node_key: Option<String>,
    /// The Node's printed identity, when the uplink carried it.
    pub serial_number: Option<String>,
    pub mac: Option<String>,
    /// RFC3339.
    pub triggered_at: String,
}

/// One job the notifier consumes.
#[derive(Debug, Clone)]
pub enum NotifyJob {
    Activated(AlarmNotifyData),
    /// An open alarm reached critical for the first time (decisions.md #26);
    /// the data is the row at its new level.
    Escalated(AlarmNotifyData),
    Cleared {
        data: AlarmNotifyData,
        /// `Some` when the alarm had been acknowledged before it cleared —
        /// the cleared email then carries the ack attribution block.
        ack: Option<AckInfo>,
    },
    Acknowledged {
        alarm: AlarmEvent,
        user: String,
        reason: String,
    },
    /// One device's merged system telemetry snapshot — the notifier itself
    /// runs it through `SystemAlertState::evaluate` (onset debounce lives
    /// here, not in the persist thread).
    SystemSnapshot {
        hostname: String,
        snapshot: SystemSnapshot,
    },
    /// Already-evaluated system alerts, for a detector that owns its own
    /// debounce and its own clock — B2's per-device offline sweep
    /// (`crate::offline`). It cannot ride `SystemSnapshot`: that path
    /// re-evaluates the Hub-scoped conditions from telemetry, and this one
    /// has no telemetry, only a verdict about one Node or Beacon.
    SystemAlerts {
        hostname: String,
        events: Vec<crate::notify::SystemAlertEvent>,
    },
}

/// Decision #19 (notification-delivery.md §6): the alarm e-mails of one Hub are collected for
/// `batch_window` from the first change and go out as one; a change beyond
/// `emails_per_alarm_per_hour` of one alarm within `rate_window` is left out.
#[derive(Debug, Clone, Copy)]
pub struct NotifierConfig {
    pub batch_window: Duration,
    pub emails_per_alarm_per_hour: usize,
    pub rate_window: Duration,
}

impl Default for NotifierConfig {
    fn default() -> Self {
        Self {
            batch_window: Duration::from_secs(30),
            emails_per_alarm_per_hour: 5,
            rate_window: Duration::from_secs(3_600),
        }
    }
}

/// A sensor-alarm e-mail waiting in its Hub's batch.
struct QueuedEmail {
    /// The alarm the limit counts: one quantity of one device.
    sensor_id: String,
    event_type: &'static str,
    notification: AlarmNotification,
}

struct Batch {
    deadline: tokio::time::Instant,
    items: Vec<QueuedEmail>,
}

/// The batches and the per-alarm counts of the alarm e-mails (§6.1, §6.2). In memory: a
/// restart starts the counts again.
struct Mailer {
    cfg: NotifierConfig,
    batches: HashMap<String, Batch>,
    sent: HashMap<(String, String), VecDeque<std::time::Instant>>,
    left_out: HashMap<(String, String), usize>,
}

impl Mailer {
    fn new(cfg: NotifierConfig) -> Self {
        Self {
            cfg,
            batches: HashMap::new(),
            sent: HashMap::new(),
            left_out: HashMap::new(),
        }
    }

    fn enqueue(&mut self, hostname: &str, item: QueuedEmail) {
        let window = self.cfg.batch_window;
        self.batches
            .entry(hostname.to_string())
            .or_insert_with(|| Batch {
                deadline: tokio::time::Instant::now() + window,
                items: Vec::new(),
            })
            .items
            .push(item);
    }

    fn next_deadline(&self) -> Option<tokio::time::Instant> {
        self.batches.values().map(|b| b.deadline).min()
    }

    /// The batches due at `now`, or all of them for `None` (the channel closed).
    fn take_due(&mut self, now: Option<tokio::time::Instant>) -> Vec<(String, Vec<QueuedEmail>)> {
        let due: Vec<String> = self
            .batches
            .iter()
            .filter(|(_, b)| now.is_none_or(|now| b.deadline <= now))
            .map(|(h, _)| h.clone())
            .collect();
        due.into_iter()
            .filter_map(|h| self.batches.remove(&h).map(|b| (h, b.items)))
            .collect()
    }

    /// Applies the per-alarm limit to a batch about to go out: each admitted change comes with
    /// the number of its alarm's changes left out since its last e-mail.
    fn admit(
        &mut self,
        hostname: &str,
        items: Vec<QueuedEmail>,
        now: std::time::Instant,
    ) -> Vec<(QueuedEmail, usize)> {
        let (limit, window) = (self.cfg.emails_per_alarm_per_hour, self.cfg.rate_window);
        let mut admitted = Vec::new();
        for item in items {
            let key = (hostname.to_string(), item.sensor_id.clone());
            let times = self.sent.entry(key.clone()).or_default();
            while times
                .front()
                .is_some_and(|t| now.duration_since(*t) >= window)
            {
                times.pop_front();
            }
            if times.len() >= limit {
                *self.left_out.entry(key).or_default() += 1;
                tracing::info!(
                    hostname,
                    sensor_id = %item.sensor_id,
                    event_type = item.event_type,
                    limit,
                    "alarm e-mail left out: the alarm reached its e-mails per hour"
                );
                continue;
            }
            times.push_back(now);
            let before = self.left_out.remove(&key).unwrap_or(0);
            admitted.push((item, before));
        }
        // Keys whose hour has passed are dropped, so the map holds only live alarms.
        if self.sent.len() > 4_096 {
            self.sent.retain(|_, t| {
                t.back()
                    .is_some_and(|last| now.duration_since(*last) < window)
            });
        }
        admitted
    }
}

/// One webhook delivery at a time, off the job loop (HN-3).
static WEBHOOK_LANE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// The notifier's connection plus the spill journal its delivery-history
/// write needs.
#[derive(Clone)]
struct NotifyDb {
    conn: crate::db::Db,
    /// Delivery history is the record that a clinician *was* told. A row lost
    /// to a locked database during a rewrite is not recoverable from anywhere
    /// else, so it spills — see `crate::durable`.
    spill: Arc<SpillGate>,
    /// The live snapshot, for the name of the Node or Beacon an alarm is on
    /// — the same name the alarm pages show.
    store: Arc<crate::state::StateStore>,
}

/// Opens (creating if needed) `<data_dir>/portal.db` on its own connection
/// and spawns the notifier loop. Returns the sender the persist thread and
/// `AlarmService` push [`NotifyJob`]s onto; dropping every sender ends the
/// task cleanly.
pub fn spawn_notifier(
    data_dir: &str,
    db_key: Option<&str>,
    spill: Arc<SpillGate>,
    hub_faults: crate::notify::HubFaults,
    store: Arc<crate::state::StateStore>,
) -> Result<mpsc::UnboundedSender<NotifyJob>, StorageError> {
    spawn_notifier_with(
        data_dir,
        db_key,
        spill,
        hub_faults,
        store,
        NotifierConfig::default(),
    )
}

/// [`spawn_notifier`] with an explicit batch window and limit (tests use a short window).
pub fn spawn_notifier_with(
    data_dir: &str,
    db_key: Option<&str>,
    spill: Arc<SpillGate>,
    hub_faults: crate::notify::HubFaults,
    store: Arc<crate::state::StateStore>,
    config: NotifierConfig,
) -> Result<mpsc::UnboundedSender<NotifyJob>, StorageError> {
    std::fs::create_dir_all(data_dir)?;
    let path = Path::new(data_dir).join("portal.db");
    let storage = Storage::open(&path, db_key)?;
    let db = NotifyDb {
        conn: crate::db::Db::from_storage(storage, "notifier"),
        spill,
        store,
    };
    let sealer = NotifySealer::new(db_key);

    let (tx, mut rx) = mpsc::unbounded_channel::<NotifyJob>();
    tokio::spawn(async move {
        let mut system_alert_states: HashMap<String, SystemAlertState> = HashMap::new();
        let mut mailer = Mailer::new(config);
        loop {
            let deadline = mailer.next_deadline();
            tokio::select! {
                job = rx.recv() => match job {
                    Some(job) => {
                        handle_job(
                            &db,
                            &sealer,
                            &mut system_alert_states,
                            &hub_faults,
                            &mut mailer,
                            job,
                        )
                        .await;
                    }
                    None => {
                        // What is still collected goes out before the task ends.
                        flush_batches(&db, &sealer, &mut mailer, None).await;
                        break;
                    }
                },
                () = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => {
                    let now = tokio::time::Instant::now();
                    flush_batches(&db, &sealer, &mut mailer, Some(now)).await;
                }
            }
        }
        tracing::info!("notifier task stopping (channel closed)");
    });

    Ok(tx)
}

async fn handle_job(
    db: &NotifyDb,
    sealer: &NotifySealer,
    states: &mut HashMap<String, SystemAlertState>,
    hub_faults: &crate::notify::HubFaults,
    mailer: &mut Mailer,
    job: NotifyJob,
) {
    match job {
        NotifyJob::Activated(data) => {
            let hostname = data.hostname.clone();
            let kind = EmailNotifyKind::Activated;
            handle_alarm(db, sealer, mailer, hostname, kind, data).await;
        }
        NotifyJob::Escalated(data) => {
            let hostname = data.hostname.clone();
            let kind = EmailNotifyKind::Escalated;
            handle_alarm(db, sealer, mailer, hostname, kind, data).await;
        }
        NotifyJob::Cleared { data, ack } => {
            let hostname = data.hostname.clone();
            let kind = EmailNotifyKind::Cleared { ack };
            handle_alarm(db, sealer, mailer, hostname, kind, data).await;
        }
        NotifyJob::Acknowledged {
            alarm,
            user,
            reason,
        } => handle_ack(db, sealer, mailer, alarm, user, reason).await,
        NotifyJob::SystemSnapshot { hostname, snapshot } => {
            handle_system_snapshot(db, sealer, states, hub_faults, hostname, snapshot).await;
        }
        NotifyJob::SystemAlerts { hostname, events } => {
            deliver_system_alerts(db, sealer, &hostname, events).await;
        }
    }
}

/// Whether a Hub's alarm e-mails reach anyone at all: its own report-config
/// row switched on with at least one recipient or, for a Hub without a row,
/// a non-empty default list (decisions.md #31). The notifications page and
/// the Hub overview mark a Hub that has neither (UX-059).
pub(crate) fn has_alarm_recipients(cfg: Option<&DeviceReportConfig>, defaults: &[String]) -> bool {
    match cfg {
        Some(c) => c.enabled && !c.recipients.is_empty(),
        None => !defaults.is_empty(),
    }
}

/// The config a Hub's alarm e-mails follow: its own row, or for a Hub
/// without one the default recipients with the toggles a new Hub row starts
/// with (activation and escalation on, clear and acknowledgement off).
fn effective_report_config(
    hostname: &str,
    cfg: Option<DeviceReportConfig>,
    defaults: Vec<String>,
) -> DeviceReportConfig {
    cfg.unwrap_or_else(|| DeviceReportConfig {
        hostname: hostname.to_string(),
        enabled: !defaults.is_empty(),
        recipients: defaults,
        notify_on_alarm: true,
        notify_on_clear: false,
        notify_on_ack: false,
        updated_at: None,
    })
}

/// True the first time it is asked about `hostname` in this process, so a
/// Hub whose alarm e-mails reach nobody is logged once, not on every alarm.
fn first_skip_for(hostname: &str) -> bool {
    static WARNED: std::sync::Mutex<std::collections::BTreeSet<String>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    WARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(hostname.to_string())
}

/// Pure gate check factored out of [`handle_alarm`]/[`handle_ack`] so the
/// report-config/notify-kind matrix is unit-testable without any I/O.
fn should_send(cfg: &DeviceReportConfig, kind: &EmailNotifyKind) -> bool {
    if !has_alarm_recipients(Some(cfg), &[]) {
        return false;
    }
    let gates = NotifyGates {
        notify_on_alarm: cfg.notify_on_alarm,
        notify_on_clear: cfg.notify_on_clear,
        notify_on_ack: cfg.notify_on_ack,
    };
    gates.allows(kind)
}

/// The `report_history.event_type` an alarm e-mail is recorded under; the
/// notifications page labels it through `alarms.event_type.*`.
fn history_event_type(kind: &EmailNotifyKind) -> &'static str {
    match kind {
        EmailNotifyKind::Activated => "alarm",
        EmailNotifyKind::Escalated => "alarm_escalated",
        EmailNotifyKind::Cleared { .. } => "alarm_clear",
        EmailNotifyKind::Acknowledged { .. } => "alarm_ack",
        EmailNotifyKind::Test => "test",
    }
}

async fn handle_alarm(
    db: &NotifyDb,
    sealer: &NotifySealer,
    mailer: &mut Mailer,
    hostname: String,
    kind: EmailNotifyKind,
    data: AlarmNotifyData,
) {
    // Webhook gating is GLOBAL ONLY (v1 parity, ws.md "Gating: global
    // config only (enabled + url)") — it must fire even when per-device
    // email reports were never configured. Email keeps the per-device
    // gates. Load global first; bail entirely only when that fails.
    let Some(global) = load_global_config(db).await else {
        return;
    };

    let event_type = history_event_type(&kind);
    let report_cfg = load_report_config(db, &hostname).await;
    let email_allowed = report_cfg
        .as_ref()
        .is_some_and(|cfg| should_send(cfg, &kind));
    let mut notification = build_notification(&data, kind, &global.email_lang, &db.store);
    notification.device_label = hub_name(db, &hostname).await;

    if email_allowed {
        // Decision #19: into the Hub's batch; `flush_batches` sends it with the others.
        mailer.enqueue(
            &hostname,
            QueuedEmail {
                sensor_id: notification.sensor_id.clone(),
                event_type,
                notification: notification.clone(),
            },
        );
    }

    deliver_webhook(db, sealer, &global, &hostname, &notification);
}

/// The Hub's name as every portal page shows it (devices.hub.md §3.1: the
/// operator's name, else the label the unit reports), or `None` for the
/// hostname fallback the subject and webhook already apply.
async fn hub_name(db: &NotifyDb, hostname: &str) -> Option<String> {
    let snapshot = db.store.snapshot();
    let device = snapshot.device(hostname);
    let meta = {
        let (h, scope) = (
            hostname.to_string(),
            portal_storage::THRESHOLD_SCOPE_HUB.to_string(),
        );
        db.conn
            .run(move |s| s.get_device_meta(&h, &scope, &h))
            .await
            .ok()
            .flatten()
    };
    crate::render::hub_display_name(device, meta.as_ref())
}

/// The webhook half of every alarm notification. Which kinds have an event
/// is [`AlarmNotification::webhook_event`]'s call: activated always;
/// escalated, cleared and acknowledged when the installation opted in
/// (`webhook_lifecycle`, decisions.md #29). Gating is global only (v1 parity, ws.md): it fires
/// even where per-device e-mail reports were never configured.
///
/// HN-3 (Hub test 2026-09-26): the webhook's retry ladder (up to minutes) ran inside the job
/// loop, so a slow receiver held back every later e-mail and system alert. It runs on its own
/// task; [`WEBHOOK_LANE`] keeps deliveries one at a time and in order. The outbox with retries
/// (notification-delivery.md) is still to come.
fn deliver_webhook(
    db: &NotifyDb,
    sealer: &NotifySealer,
    global: &portal_storage::NotifyGlobalConfig,
    hostname: &str,
    notification: &AlarmNotification,
) {
    if !global.webhook_enabled {
        return;
    }
    let (Some(webhook_cfg), Some(payload)) = (
        build_webhook_config(global, sealer),
        notification.webhook_payload(global.webhook_lifecycle),
    ) else {
        return;
    };
    let db = db.clone();
    let hostname = hostname.to_string();
    tokio::spawn(async move {
        let _lane = WEBHOOK_LANE.acquire().await;
        let outcome = send_webhook(&webhook_cfg, &payload).await;
        record_history(
            &db,
            Some(hostname),
            "webhook_alarm",
            &[],
            None,
            Some(format!(
                "webhook {} delivery to {}",
                payload["event"].as_str().unwrap_or("alarm"),
                crate::notify::webhook::redact_url(&webhook_cfg.url)
            )),
            outcome.success,
            (!outcome.success).then(|| outcome.message.clone()),
            Some("webhook".to_string()),
            HISTORY_SOURCE_REPORT,
        )
        .await;
    });
}

async fn handle_ack(
    db: &NotifyDb,
    sealer: &NotifySealer,
    mailer: &mut Mailer,
    alarm: AlarmEvent,
    user: String,
    reason: String,
) {
    let hostname = alarm.hostname.clone();
    let Some(global) = load_global_config(db).await else {
        return;
    };
    // One lookup: the e-mail and the webhook's `acknowledgement.name` both
    // show the person, the webhook's `user` keeps the username.
    let names = db.conn.full_names(vec![user.clone()]).await;
    let kind = EmailNotifyKind::Acknowledged {
        name: crate::render::person_label(&user, &names),
        user,
        reason: Some(reason),
    };
    let report_cfg = load_report_config(db, &hostname).await;
    let email_allowed = report_cfg
        .as_ref()
        .is_some_and(|cfg| should_send(cfg, &kind));
    let event_type = history_event_type(&kind);
    let data = ack_notify_data(&alarm, &db.store.snapshot());
    let mut notification = build_notification(&data, kind, &global.email_lang, &db.store);
    notification.device_label = hub_name(db, &hostname).await;

    if email_allowed {
        mailer.enqueue(
            &hostname,
            QueuedEmail {
                sensor_id: notification.sensor_id.clone(),
                event_type,
                notification: notification.clone(),
            },
        );
    }
    deliver_webhook(db, sealer, &global, &hostname, &notification);
}

/// Sends the due batches (all of them for `now == None`): the per-alarm limit first, then one
/// e-mail per Hub to its current recipients (decision #19).
async fn flush_batches(
    db: &NotifyDb,
    sealer: &NotifySealer,
    mailer: &mut Mailer,
    now: Option<tokio::time::Instant>,
) {
    for (hostname, items) in mailer.take_due(now) {
        let Some(global) = load_global_config(db).await else {
            continue;
        };
        // The recipients as they are now; a report switched off meanwhile sends nothing.
        let Some(report_cfg) = load_report_config(db, &hostname)
            .await
            .filter(|cfg| has_alarm_recipients(Some(cfg), &[]))
        else {
            continue;
        };
        let admitted = mailer.admit(&hostname, items, std::time::Instant::now());
        if admitted.is_empty() {
            continue;
        }
        let limit = mailer.cfg.emails_per_alarm_per_hour;
        let parts: Vec<(AlarmNotification, usize)> = admitted
            .iter()
            .map(|(q, left_out)| (q.notification.clone(), *left_out))
            .collect();
        let (subject, text, html) =
            crate::notify::payload::batch_email(&parts, limit, global.public_url.as_deref());
        let (event_type, message) = match admitted.as_slice() {
            [(single, _)] => (single.event_type, single.notification.message.clone()),
            many => (
                "alarm_batch",
                many.iter()
                    .map(|(q, _)| format!("{} ({})", q.notification.sensor_name, q.event_type))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        };
        // Attempted (and recorded) even when `provider == "none"` — an
        // operator who enabled per-device reports but never finished the
        // global email setup should see that failure in `report_history`,
        // not silence (matches `handle_system_snapshot`'s same choice below).
        let transport = build_transport(&global, sealer);
        let from = global.from_email.clone().unwrap_or_default();
        let outcome = transport
            .send(
                &from,
                crate::notify::from_name(&global.from_name),
                &report_cfg.recipients,
                &subject,
                &text,
                &html,
            )
            .await;
        record_history(
            db,
            Some(hostname),
            event_type,
            &report_cfg.recipients,
            Some(subject),
            Some(message),
            outcome.success,
            (!outcome.success).then(|| outcome.message.clone()),
            Some(global.provider.clone()),
            HISTORY_SOURCE_REPORT,
        )
        .await;
    }
}

/// An acknowledged alarm's facts, from its stored row. The row has no
/// device identity columns, so the Node's dev_eui and serial or the
/// Beacon's MAC, which `persist.rs` hands the activation and clear paths
/// from the uplink, are read back off the `sensor_id` convention and the
/// live snapshot. Without them `alarm.acknowledged` carried
/// `dev_eui: null` where `alarm.activated` had it (UX-028).
fn ack_notify_data(alarm: &AlarmEvent, state: &crate::state::PortalState) -> AlarmNotifyData {
    use crate::alarms::AlarmTarget;
    let (node_key, serial_number, mac) = match crate::alarms::alarm_target(&alarm.sensor_id) {
        AlarmTarget::Node(key) => {
            let serial = state
                .hub_of_node(&key)
                .and_then(|(_, n)| n.serial_number.clone());
            (Some(key), serial, None)
        }
        AlarmTarget::Beacon(mac) => (None, None, Some(mac)),
        AlarmTarget::Line(_) | AlarmTarget::Hub => (None, None, None),
    };
    AlarmNotifyData {
        alarm_id: alarm.id.clone(),
        hostname: alarm.hostname.clone(),
        severity: alarm.severity.clone(),
        alarm_type: alarm.alarm_type.clone(),
        source: source_from_sensor_id(&alarm.sensor_id),
        value: alarm.value,
        threshold: alarm.threshold,
        message: alarm.message.clone(),
        sensor_name: alarm
            .sensor_name
            .clone()
            .unwrap_or_else(|| alarm.sensor_id.clone()),
        sensor_id: alarm.sensor_id.clone(),
        line: line_from_sensor_id(&alarm.sensor_id),
        location: alarm.sensor_location.clone(),
        node_key,
        serial_number,
        mac,
        triggered_at: alarm.triggered_at.clone(),
    }
}

async fn handle_system_snapshot(
    db: &NotifyDb,
    sealer: &NotifySealer,
    states: &mut HashMap<String, SystemAlertState>,
    hub_faults: &crate::notify::HubFaults,
    hostname: String,
    snapshot: SystemSnapshot,
) {
    let cfg = match db.conn.run(|s| s.get_system_alert_config()).await {
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::warn!(%err, "failed to load system alert config");
            return;
        }
    };
    let checker_cfg = map_system_alert_config(&cfg);
    let state = states.entry(hostname.clone()).or_default();
    let events = state.evaluate(&snapshot, &checker_cfg);
    hub_faults.set(&hostname, state.active());
    deliver_system_alerts(db, sealer, &hostname, events).await;
}

/// Emails and records a batch of already-evaluated system alerts.
///
/// Split out of [`handle_system_snapshot`] for B2: the per-device offline
/// detector does its own evaluation on its own clock (`crate::offline`), so
/// it arrives here holding events rather than a snapshot — but the delivery
/// half (recipients, provider, history row) has to be the one the Hub-scoped
/// alerts already use, or the two would drift on which failures get a
/// history row.
async fn deliver_system_alerts(
    db: &NotifyDb,
    sealer: &NotifySealer,
    hostname: &str,
    events: Vec<crate::notify::SystemAlertEvent>,
) {
    let cfg = match db.conn.run(|s| s.get_system_alert_config()).await {
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::warn!(%err, "failed to load system alert config");
            return;
        }
    };
    if events.is_empty() || cfg.recipients.is_empty() {
        return;
    }
    let hostname = hostname.to_string();

    let Some(global) = load_global_config(db).await else {
        return;
    };

    let transport = build_transport(&global, sealer);
    let from = global.from_email.clone().unwrap_or_default();
    for event in events {
        let subject = event.email_subject(&hostname, &global.email_lang);
        let body = event.email_body(&hostname, &global.email_lang, global.public_url.as_deref());
        let outcome = if global.provider == "none" {
            crate::notify::email::SendOutcome {
                success: false,
                message: "no email provider configured".to_string(),
            }
        } else {
            transport
                .send(
                    &from,
                    crate::notify::from_name(&global.from_name),
                    &cfg.recipients,
                    &subject,
                    &body.to_text(),
                    &body.to_html(),
                )
                .await
        };
        record_history(
            db,
            Some(hostname.clone()),
            event.kind.as_str(),
            &cfg.recipients,
            Some(subject),
            Some(event.message.clone()),
            outcome.success,
            (!outcome.success).then(|| outcome.message.clone()),
            Some(global.provider.clone()),
            HISTORY_SOURCE_SYSTEM_ALERT,
        )
        .await;
    }
}

/// A relay's error text with every e-mail address blanked (R-14, review of !102): an SMTP answer
/// such as `550 <jan@x.cz>: user unknown` names the recipient, and the journal is not the place
/// for addresses. The history row keeps the full text, visible only in the Portal.
fn redact_addresses(text: &str) -> String {
    text.split(' ')
        .map(|word| {
            if word.contains('@') {
                "<address>"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The alarm jobs' per-Hub e-mail config: the Hub's own row, else the
/// default recipients ([`effective_report_config`]). A Hub with no active
/// recipients ([`has_alarm_recipients`]) gets no alarm e-mail, which is
/// logged once per Hub per process (UX-059) instead of passing in silence.
async fn load_report_config(db: &NotifyDb, hostname: &str) -> Option<DeviceReportConfig> {
    let host = hostname.to_string();
    match db
        .conn
        .run(move |s| {
            let cfg = s.get_device_report_config(&host)?;
            let defaults = match cfg {
                Some(_) => Vec::new(),
                None => s.default_alarm_recipients()?,
            };
            Ok((cfg, defaults))
        })
        .await
    {
        Ok((cfg, defaults)) => {
            if !has_alarm_recipients(cfg.as_ref(), &defaults) && first_skip_for(hostname) {
                tracing::warn!(
                    hostname,
                    "alarm e-mail skipped: the Hub has no active recipients \
                     (logged once per Hub until restart)"
                );
            }
            Some(effective_report_config(hostname, cfg, defaults))
        }
        Err(err) => {
            tracing::warn!(%err, "failed to load device report config");
            None
        }
    }
}

async fn load_global_config(db: &NotifyDb) -> Option<NotifyGlobalConfig> {
    match db.conn.run(|s| s.get_notify_global_config()).await {
        Ok(cfg) => Some(cfg),
        Err(err) => {
            tracing::warn!(%err, "failed to load notify global config");
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn record_history(
    db: &NotifyDb,
    hostname: Option<String>,
    event_type: &str,
    recipients: &[String],
    subject: Option<String>,
    message: Option<String>,
    success: bool,
    error_message: Option<String>,
    provider: Option<String>,
    source: &'static str,
) {
    // HN-8: the journal says what happened to every delivery, not only the history table.
    // Counts, not addresses: the log is not the place for recipients.
    if success {
        tracing::info!(
            event_type,
            recipients = recipients.len(),
            provider = provider.as_deref().unwrap_or(""),
            "notification delivered"
        );
    } else {
        tracing::warn!(
            event_type,
            recipients = recipients.len(),
            provider = provider.as_deref().unwrap_or(""),
            error = %redact_addresses(error_message.as_deref().unwrap_or("")),
            "notification not delivered"
        );
    }
    let row = NewReportHistory {
        hostname,
        event_type: event_type.to_string(),
        recipients: recipients.to_vec(),
        subject,
        message,
        status: if success {
            HISTORY_STATUS_SENT.to_string()
        } else {
            HISTORY_STATUS_FAILED.to_string()
        },
        error_message,
        provider,
        source: source.to_string(),
        sent_at: None,
    };
    let spill = Arc::clone(&db.spill);
    if let Err(err) = db
        .conn
        .run(move |s| crate::durable::insert_report_history(&spill, s, &row).map(|_| ()))
        .await
    {
        tracing::warn!(%err, "failed to record report history");
    }
}

fn build_notification(
    data: &AlarmNotifyData,
    kind: EmailNotifyKind,
    lang: &str,
    store: &crate::state::StateStore,
) -> AlarmNotification {
    let field = crate::alarms::alarm_field(&data.sensor_id, Some(&data.sensor_name));
    AlarmNotification {
        kind,
        severity: data.severity.clone(),
        alarm_type: data.alarm_type.clone(),
        source: data.source,
        value: data.value,
        threshold: data.threshold,
        message: data.message.clone().unwrap_or_default(),
        hostname: data.hostname.clone(),
        device_label: None,
        // `node_key` is the dev_eui on both transports (GitLab #73), so it always
        // fills the frozen v1 `dev_eui` field; `serial_number` rides alongside as
        // the printed identity whenever the uplink carried one.
        dev_eui: data.node_key.clone(),
        serial_number: data.serial_number.clone(),
        mac: data.mac.clone(),
        sensor_name: data.sensor_name.clone(),
        sensor_id: data.sensor_id.clone(),
        line: data.line,
        location: data.location.clone(),
        triggered_at: parse_iso(&data.triggered_at),
        alarm_id: data.alarm_id.clone(),
        lang: lang.to_string(),
        quantity: field.map(|f| f.name.to_string()),
        unit: field.and_then(|f| f.unit).map(str::to_string),
        device_name: crate::alarms::alarm_device_name(&data.sensor_id, &store.snapshot()),
    }
}

fn parse_iso(s: &str) -> OffsetDateTime {
    OffsetDateTime::parse(s, &Rfc3339).unwrap_or_else(|_| OffsetDateTime::now_utc())
}

/// Reconstructs the notify-facing source from the stored `sensor_id`
/// convention (`line-N` / `lrw:*` / `eye:*` / `sys:*`) — used only
/// for the acknowledged-notification path, which has just the stored row (no
/// engine context) to work from. `lrw:` is every Node (STICKER) alarm, on
/// either carrier (`portal_domain::node_sensor_prefix`).
pub(crate) fn source_from_sensor_id(sensor_id: &str) -> NotifyAlarmSource {
    if sensor_id.starts_with("line-") {
        NotifyAlarmSource::Fiber
    } else if portal_domain::is_node_sensor_id(sensor_id)
        || sensor_id.starts_with(crate::pending::CONFIG_WARNING_PREFIX)
    {
        NotifyAlarmSource::Sticker
    } else if sensor_id.starts_with("eye:") {
        NotifyAlarmSource::Eye
    } else {
        NotifyAlarmSource::System
    }
}

fn line_from_sensor_id(sensor_id: &str) -> Option<i32> {
    sensor_id.strip_prefix("line-").and_then(|n| n.parse().ok())
}

fn map_system_alert_config(c: &portal_storage::SystemAlertConfig) -> CheckerSystemAlertConfig {
    CheckerSystemAlertConfig {
        battery_enabled: c.battery_enabled,
        battery_threshold: c.battery_threshold as f64,
        storage_enabled: c.storage_enabled,
        storage_threshold: c.storage_threshold as f64,
        // No column of its own (v1 quirk #5). The Portal's data sits on the Hub's `/data`, so
        // it follows the device storage switch and threshold (R-5, review of !102).
        portal_storage_enabled: c.storage_enabled,
        portal_storage_threshold: c.storage_threshold as f64,
        network_enabled: c.network_enabled,
        offline_enabled: c.device_offline_enabled,
        version_enabled: c.version_mismatch_enabled,
        expected_version: c.expected_version.clone(),
    }
}

fn build_transport(cfg: &NotifyGlobalConfig, sealer: &NotifySealer) -> EmailTransport {
    match cfg.provider.as_str() {
        "smtp" => {
            let password = cfg
                .smtp_password_enc
                .as_ref()
                .and_then(|enc| sealer.unseal_string(enc).ok())
                .map(|z| z.to_string());
            EmailTransport::Smtp {
                host: cfg.smtp_host.clone().unwrap_or_default(),
                port: cfg.smtp_port.unwrap_or(587).clamp(1, u16::MAX as i64) as u16,
                username: cfg.smtp_username.clone(),
                password,
                use_tls: cfg.smtp_use_tls,
                use_ssl: cfg.smtp_use_ssl,
            }
        }
        "sendgrid" => {
            let api_key = cfg
                .sendgrid_api_key_enc
                .as_ref()
                .and_then(|enc| sealer.unseal_string(enc).ok())
                .map(|z| z.to_string())
                .unwrap_or_default();
            EmailTransport::Sendgrid { api_key }
        }
        _ => EmailTransport::None,
    }
}

/// The sealed webhook headers as the settings form stores them: a JSON list of `[name, value]`
/// pairs (`notification_routes::merge_webhook_form`). HN-1 (night test 2026-09-26): this read
/// them as a JSON object, failed on every saved header and sent real alarm webhooks without
/// them while the test button, which reads the list, sent them. An object is still accepted.
pub(crate) fn decode_webhook_headers(bytes: &[u8]) -> Option<Vec<(String, String)>> {
    serde_json::from_slice::<Vec<(String, String)>>(bytes)
        .ok()
        .or_else(|| {
            serde_json::from_slice::<HashMap<String, String>>(bytes)
                .ok()
                .map(|m| m.into_iter().collect())
        })
}

fn build_webhook_config(cfg: &NotifyGlobalConfig, sealer: &NotifySealer) -> Option<WebhookConfig> {
    if !cfg.webhook_enabled {
        return None;
    }
    let url = cfg.webhook_url.clone()?;
    let headers = match cfg.webhook_headers_enc.as_ref() {
        None => Vec::new(),
        Some(enc) => match sealer
            .unseal(enc)
            .ok()
            .and_then(|b| decode_webhook_headers(&b))
        {
            Some(headers) => headers,
            None => {
                tracing::warn!("webhook headers unreadable; sending without them");
                Vec::new()
            }
        },
    };
    Some(WebhookConfig {
        url,
        method: cfg.webhook_method.clone(),
        headers,
        timeout_s: cfg.webhook_timeout_s.clamp(1, 60) as u64,
        max_attempts: cfg.webhook_max_attempts.clamp(1, 10) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use portal_storage::ReportHistoryFilter;

    fn queued(sensor: &str, event_type: &'static str) -> QueuedEmail {
        let data = AlarmNotifyData {
            alarm_id: format!("{sensor}-{event_type}"),
            hostname: "h1".into(),
            severity: "warning".into(),
            alarm_type: "HIGH".into(),
            source: NotifyAlarmSource::Fiber,
            value: Some(9.0),
            threshold: Some(8.0),
            message: None,
            sensor_name: sensor.into(),
            sensor_id: sensor.into(),
            line: None,
            location: None,
            node_key: None,
            serial_number: None,
            mac: None,
            triggered_at: "2026-09-26T00:00:00Z".into(),
        };
        QueuedEmail {
            sensor_id: sensor.into(),
            event_type,
            notification: build_notification(
                &data,
                EmailNotifyKind::Activated,
                "en",
                &crate::state::StateStore::new(),
            ),
        }
    }

    /// Decision #19 §6.2: five e-mails per alarm per hour; the sixth change is left out and
    /// the next admitted one says how many were; another alarm is not affected.
    #[test]
    fn an_alarm_is_e_mailed_at_most_five_times_an_hour() {
        let mut mailer = Mailer::new(NotifierConfig::default());
        let t0 = std::time::Instant::now();
        for i in 0..5 {
            let admitted = mailer.admit("h1", vec![queued("line-0", "alarm")], t0);
            assert_eq!(admitted.len(), 1, "change {i}");
            assert_eq!(admitted[0].1, 0);
        }
        assert!(mailer
            .admit("h1", vec![queued("line-0", "alarm_clear")], t0)
            .is_empty());
        assert!(mailer
            .admit("h1", vec![queued("line-0", "alarm")], t0)
            .is_empty());
        assert_eq!(
            mailer
                .admit("h1", vec![queued("line-1", "alarm")], t0)
                .len(),
            1,
            "another alarm has its own five"
        );
        let later = t0 + Duration::from_secs(3_601);
        let admitted = mailer.admit("h1", vec![queued("line-0", "alarm")], later);
        assert_eq!(admitted.len(), 1, "the hour freed a slot");
        assert_eq!(
            admitted[0].1, 2,
            "and the e-mail says two changes were left out"
        );
    }

    /// The notifier with a batch window short enough for a test to wait out (decision #19's
    /// 30 s is the production default).
    fn spawn_notifier(
        data_dir: &str,
        db_key: Option<&str>,
        spill: Arc<SpillGate>,
        hub_faults: crate::notify::HubFaults,
        store: Arc<crate::state::StateStore>,
    ) -> Result<mpsc::UnboundedSender<NotifyJob>, StorageError> {
        spawn_notifier_with(
            data_dir,
            db_key,
            spill,
            hub_faults,
            store,
            NotifierConfig {
                batch_window: Duration::from_millis(20),
                ..NotifierConfig::default()
            },
        )
    }

    /// R-14: the journal line has no address in it.
    #[test]
    fn a_delivery_error_is_logged_without_addresses() {
        assert_eq!(
            redact_addresses("550 <jan@x.cz>: user unknown"),
            "550 <address> user unknown"
        );
        assert_eq!(redact_addresses("timed out"), "timed out");
    }

    /// HN-1: the settings form seals the headers as a list of pairs; the real alarm path must
    /// read that, and still take the older object shape.
    #[test]
    fn the_saved_webhook_headers_are_what_the_alarm_path_sends() {
        let pairs = crate::routes::notification_routes::parse_headers(
            "Authorization: Bearer x\nX-Site: hala",
        )
        .expect("headers parse");
        let blob = serde_json::to_vec(&pairs).expect("json");
        assert_eq!(decode_webhook_headers(&blob), Some(pairs));
        let object = br#"{"Authorization":"Bearer x"}"#;
        assert_eq!(
            decode_webhook_headers(object),
            Some(vec![("Authorization".to_string(), "Bearer x".to_string())])
        );
        assert_eq!(decode_webhook_headers(b"not json"), None);
    }

    fn report_cfg(
        enabled: bool,
        recipients: &[&str],
        on_alarm: bool,
        on_clear: bool,
        on_ack: bool,
    ) -> DeviceReportConfig {
        DeviceReportConfig {
            hostname: "h1".into(),
            recipients: recipients.iter().map(|s| s.to_string()).collect(),
            enabled,
            notify_on_alarm: on_alarm,
            notify_on_clear: on_clear,
            notify_on_ack: on_ack,
            updated_at: None,
        }
    }

    // ---- pure gate matrix (no I/O) ----

    #[test]
    fn disabled_or_recipientless_never_sends() {
        let activated = EmailNotifyKind::Activated;
        assert!(!should_send(
            &report_cfg(false, &["a@b.c"], true, true, true),
            &activated
        ));
        assert!(!should_send(
            &report_cfg(true, &[], true, true, true),
            &activated
        ));
    }

    /// UX-059: a skipped alarm e-mail is logged once per Hub, not per alarm.
    #[test]
    fn a_hub_without_recipients_is_warned_about_once() {
        assert!(!has_alarm_recipients(None, &[]));
        assert!(first_skip_for("ux059-hub-a"));
        assert!(!first_skip_for("ux059-hub-a"));
        assert!(first_skip_for("ux059-hub-b"));
    }

    #[test]
    fn activated_defaults_on_clear_and_ack_default_off() {
        let cfg = report_cfg(true, &["a@b.c"], true, false, false);
        assert!(should_send(&cfg, &EmailNotifyKind::Activated));
        assert!(!should_send(&cfg, &EmailNotifyKind::Cleared { ack: None }));
        assert!(!should_send(
            &cfg,
            &EmailNotifyKind::Acknowledged {
                user: "a".into(),
                name: "a".into(),
                reason: None
            }
        ));
    }

    #[test]
    fn per_kind_toggles_gate_independently() {
        let cfg = report_cfg(true, &["a@b.c"], false, true, true);
        assert!(!should_send(&cfg, &EmailNotifyKind::Activated));
        assert!(should_send(&cfg, &EmailNotifyKind::Cleared { ack: None }));
        assert!(should_send(
            &cfg,
            &EmailNotifyKind::Acknowledged {
                user: "a".into(),
                name: "a".into(),
                reason: None
            }
        ));
    }

    #[test]
    fn sensor_id_source_and_line_roundtrip() {
        assert!(matches!(
            source_from_sensor_id("line-3"),
            NotifyAlarmSource::Fiber
        ));
        assert!(matches!(
            source_from_sensor_id("lrw:aa:Temp"),
            NotifyAlarmSource::Sticker
        ));
        assert!(matches!(
            source_from_sensor_id("eye:AA:BB:Temp"),
            NotifyAlarmSource::Eye
        ));
        assert!(matches!(
            source_from_sensor_id("sys:power"),
            NotifyAlarmSource::System
        ));
        assert_eq!(line_from_sensor_id("line-3"), Some(3));
        assert_eq!(line_from_sensor_id("sys:power"), None);
    }

    /// Both identities go out on every Node alarm, whatever the transport: `node_key` is
    /// the dev_eui on LoRaWAN and P2P alike (GitLab #73), and the serial is the printed
    /// label a receiver may want to show.
    #[test]
    fn a_node_alarm_carries_dev_eui_and_serial_on_both_transports() {
        let data = |sensor_id: &str| AlarmNotifyData {
            alarm_id: "1".into(),
            hostname: "h1".into(),
            severity: "warning".into(),
            alarm_type: "HIGH".into(),
            source: NotifyAlarmSource::Sticker,
            value: Some(1.0),
            threshold: Some(0.5),
            message: None,
            sensor_name: "Temp".into(),
            sensor_id: sensor_id.into(),
            line: None,
            location: None,
            node_key: Some("abc123".into()),
            serial_number: Some("SN-1".into()),
            mac: None,
            triggered_at: "2026-08-19T10:00:00Z".into(),
        };

        for sensor_id in ["lrw:abc123:Temp", "p2p:abc123:Temp"] {
            let store = crate::state::StateStore::new();
            let n = build_notification(&data(sensor_id), EmailNotifyKind::Activated, "en", &store);
            assert_eq!(n.dev_eui.as_deref(), Some("abc123"), "{sensor_id}");
            assert_eq!(n.serial_number.as_deref(), Some("SN-1"), "{sensor_id}");

            let body = n
                .webhook_payload(false)
                .expect("activated has a webhook event");
            assert_eq!(body["device"]["dev_eui"], "abc123", "{sensor_id}");
            assert_eq!(body["device"]["serial_number"], "SN-1", "{sensor_id}");
        }
    }

    /// UX-028: the acknowledgement is built from the stored row, which has no
    /// identity columns, and still names the device the activation named.
    #[test]
    fn an_acknowledged_alarm_names_the_same_device_as_its_activation() {
        let mut node = crate::state::NodeState::new("abc123".into(), 0);
        node.serial_number = Some("SN-1".into());
        let mut device = crate::state::DeviceState::new("h1".into());
        device.nodes.insert("abc123".into(), node);
        let mut state = crate::state::PortalState::default();
        state.devices.insert("h1".into(), device);

        let alarm = |sensor_id: &str| AlarmEvent {
            id: "a1".into(),
            hostname: "h1".into(),
            sensor_id: sensor_id.into(),
            sensor_name: Some("Temperature".into()),
            sensor_location: None,
            alarm_type: "HIGH".into(),
            severity: "warning".into(),
            from_state: None,
            to_state: None,
            message: None,
            value: Some(9.0),
            threshold: Some(8.0),
            triggered_at: "2026-08-18T00:00:00Z".into(),
            cleared_at: None,
            acknowledged_at: Some("2026-08-18T00:05:00Z".into()),
            acknowledged_by_user_id: Some(1),
            acknowledged_by_username: Some("alice".into()),
            native_key: None,
            created_at: "2026-08-18T00:00:00Z".into(),
            max_severity: "warning".into(),
            end_reason: None,
        };
        let ack = || EmailNotifyKind::Acknowledged {
            user: "alice".into(),
            name: "alice".into(),
            reason: None,
        };
        let store = crate::state::StateStore::new();

        let data = ack_notify_data(&alarm("lrw:abc123:Temperature"), &state);
        let body = build_notification(&data, ack(), "en", &store)
            .webhook_payload(true)
            .expect("acknowledged has a webhook event when lifecycle is on");
        assert_eq!(body["device"]["dev_eui"], "abc123");
        assert_eq!(body["device"]["serial_number"], "SN-1");

        let data = ack_notify_data(&alarm("eye:AA:BB:CC:DD:EE:FF:Temperature"), &state);
        assert_eq!(data.mac.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
    }

    /// UX-040: a Beacon alarm's e-mail names the Beacon and puts the unit
    /// on the value and the limit, although the MAC in its `sensor_id` has
    /// colons of its own.
    #[test]
    fn a_beacon_alarm_email_names_the_beacon_and_the_unit() {
        let store = crate::state::StateStore::new();
        store.apply(crate::state::IngestEvent::EyeSensors {
            hostname: "h1".into(),
            payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                timestamp: None,
                discovered_capped: false,
                tags: vec![serde_json::from_value(serde_json::json!({
                    "mac": "EE:E0:00:00:00:01", "name": "Freezer", "temperature_c": 4.44
                }))
                .expect("tag")],
            },
        });
        let data = AlarmNotifyData {
            alarm_id: "a1".into(),
            hostname: "h1".into(),
            severity: "critical".into(),
            alarm_type: "HIGH".into(),
            source: NotifyAlarmSource::Eye,
            value: Some(4.44),
            threshold: Some(-10.0),
            message: None,
            sensor_name: "Temperature".into(),
            sensor_id: "eye:EE:E0:00:00:00:01:Temperature".into(),
            line: None,
            location: None,
            node_key: None,
            serial_number: None,
            mac: Some("EE:E0:00:00:00:01".into()),
            triggered_at: "2026-09-26T05:37:51Z".into(),
        };
        let n = build_notification(&data, EmailNotifyKind::Activated, "en", &store);
        let subject = n.email_subject();
        assert!(subject.contains("Freezer"), "{subject}");
        let html = n.email_body(None).to_html();
        assert!(html.contains("4.4 \u{00b0}C"), "{html}");
        assert!(html.contains("-10.0 \u{00b0}C"), "{html}");
    }

    /// Every kind the notifier records has a label on the notifications page;
    /// the history renders `alarms.event_type.<kind>` and a missing entry
    /// puts the raw key on screen (UX-022).
    #[test]
    fn every_alarm_email_event_type_has_a_label() {
        for kind in [
            EmailNotifyKind::Activated,
            EmailNotifyKind::Escalated,
            EmailNotifyKind::Cleared { ack: None },
            EmailNotifyKind::Acknowledged {
                user: "a".into(),
                name: "a".into(),
                reason: None,
            },
            EmailNotifyKind::Test,
        ] {
            let key = format!("alarms.event_type.{}", history_event_type(&kind));
            for lang in ["cs", "en"] {
                assert_ne!(crate::i18n::lookup(lang, &key), key, "{lang}");
            }
        }
    }

    // ---- task-seam integration (temp Storage, no network: provider stays
    // "none" so every send fails softly but report_history still records
    // the attempt — enough to prove the plumbing without a live SMTP/HTTP
    // server) ----

    fn open_direct(dir: &std::path::Path) -> Storage {
        Storage::open(&dir.join("portal.db"), None).expect("open storage")
    }

    #[tokio::test]
    async fn activated_alarm_notify_job_records_history_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        {
            let storage = open_direct(dir.path());
            storage
                .upsert_device_report_config(&report_cfg(
                    true,
                    &["ops@example.com"],
                    true,
                    false,
                    false,
                ))
                .unwrap();
        }
        let tx = spawn_notifier(
            dir.path().to_str().unwrap(),
            None,
            crate::durable::test_gate(dir.path().to_str().unwrap()),
            crate::notify::HubFaults::default(),
            Arc::new(crate::state::StateStore::new()),
        )
        .expect("spawn notifier");

        tx.send(NotifyJob::Activated(AlarmNotifyData {
            alarm_id: "a1".into(),
            hostname: "h1".into(),
            severity: "warning".into(),
            alarm_type: "HIGH".into(),
            source: NotifyAlarmSource::Fiber,
            value: Some(9.0),
            threshold: Some(8.0),
            message: None,
            sensor_name: "Fridge".into(),
            sensor_id: "line-0".into(),
            line: Some(0),
            location: None,
            node_key: None,
            serial_number: None,
            mac: None,
            triggered_at: "2026-08-18T00:00:00Z".into(),
        }))
        .unwrap();

        // Wait for the spawned task to write its row, bounded: a fixed 100 ms was not
        // always enough on a loaded runner with the tests running in parallel.
        let storage = open_direct(dir.path());
        let mut listed = storage
            .list_report_history(&ReportHistoryFilter::default())
            .unwrap();
        for _ in 0..500 {
            if listed.1 > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            listed = storage
                .list_report_history(&ReportHistoryFilter::default())
                .unwrap();
        }
        drop(tx);

        let (rows, total) = listed;
        assert_eq!(total, 1);
        assert_eq!(rows[0].event_type, "alarm");
        assert_eq!(rows[0].status, "failed", "provider none: send fails softly");
    }

    /// HN-3: a webhook receiver that accepts and never answers must not hold back the next
    /// alarm's e-mail. The webhook runs on its own task; the second alarm's history row lands
    /// while the first webhook still waits out its timeout.
    #[tokio::test]
    async fn a_hanging_webhook_does_not_hold_back_the_next_alarm() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept and hold every connection without answering.
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        {
            let storage = open_direct(dir.path());
            storage
                .upsert_device_report_config(&report_cfg(
                    true,
                    &["ops@example.com"],
                    true,
                    false,
                    false,
                ))
                .unwrap();
            let mut global = storage.get_notify_global_config().unwrap();
            global.webhook_enabled = true;
            global.webhook_url = Some(format!("http://{addr}/hook"));
            global.webhook_timeout_s = 20;
            global.webhook_max_attempts = 1;
            storage.upsert_notify_global_config(&global).unwrap();
        }
        let tx = spawn_notifier(
            dir.path().to_str().unwrap(),
            None,
            crate::durable::test_gate(dir.path().to_str().unwrap()),
            crate::notify::HubFaults::default(),
            Arc::new(crate::state::StateStore::new()),
        )
        .expect("spawn notifier");
        let alarm = |id: &str| AlarmNotifyData {
            alarm_id: id.into(),
            hostname: "h1".into(),
            severity: "warning".into(),
            alarm_type: "HIGH".into(),
            source: NotifyAlarmSource::Fiber,
            value: Some(9.0),
            threshold: Some(8.0),
            message: None,
            sensor_name: "Fridge".into(),
            sensor_id: "line-0".into(),
            line: Some(0),
            location: None,
            node_key: None,
            serial_number: None,
            mac: None,
            triggered_at: "2026-08-18T00:00:00Z".into(),
        };
        tx.send(NotifyJob::Activated(alarm("a1"))).unwrap();
        tx.send(NotifyJob::Activated(alarm("a2"))).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let storage = open_direct(dir.path());
        let (rows, _) = storage
            .list_report_history(&ReportHistoryFilter::default())
            .unwrap();
        // Decision #19: the two raises of one Hub within the batch window are one e-mail.
        let emails = rows
            .iter()
            .filter(|r| r.event_type == "alarm_batch")
            .count();
        assert_eq!(
            emails, 1,
            "the batch was attempted while the webhook hangs: {rows:?}"
        );
        drop(tx);
    }

    #[tokio::test]
    async fn disabled_device_report_config_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let tx = spawn_notifier(
            dir.path().to_str().unwrap(),
            None,
            crate::durable::test_gate(dir.path().to_str().unwrap()),
            crate::notify::HubFaults::default(),
            Arc::new(crate::state::StateStore::new()),
        )
        .expect("spawn notifier");

        tx.send(NotifyJob::Activated(AlarmNotifyData {
            alarm_id: "a1".into(),
            hostname: "h1".into(),
            severity: "warning".into(),
            alarm_type: "HIGH".into(),
            source: NotifyAlarmSource::Fiber,
            value: None,
            threshold: None,
            message: None,
            sensor_name: "Fridge".into(),
            sensor_id: "line-0".into(),
            line: Some(0),
            location: None,
            node_key: None,
            serial_number: None,
            mac: None,
            triggered_at: "2026-08-18T00:00:00Z".into(),
        }))
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(tx);

        let storage = open_direct(dir.path());
        let (_rows, total) = storage
            .list_report_history(&ReportHistoryFilter::default())
            .unwrap();
        assert_eq!(
            total, 0,
            "no device_report_config row — never configured, never sent"
        );
    }

    fn alarm_data(hostname: &str) -> AlarmNotifyData {
        AlarmNotifyData {
            alarm_id: "a1".into(),
            hostname: hostname.into(),
            severity: "critical".into(),
            alarm_type: "HIGH".into(),
            source: NotifyAlarmSource::Fiber,
            value: Some(9.0),
            threshold: Some(8.0),
            message: None,
            sensor_name: "Fridge".into(),
            sensor_id: "line-0".into(),
            line: Some(0),
            location: None,
            node_key: None,
            serial_number: None,
            mac: None,
            triggered_at: "2026-08-18T00:00:00Z".into(),
        }
    }

    /// Runs `jobs` through a fresh notifier over `dir` and returns the
    /// delivery history once `expect` rows are in (or after a short wait
    /// when `expect` is 0, where waiting longer proves nothing more).
    async fn run_jobs(
        dir: &std::path::Path,
        jobs: Vec<NotifyJob>,
        expect: i64,
    ) -> Vec<portal_storage::ReportHistoryEntry> {
        let tx = spawn_notifier(
            dir.to_str().unwrap(),
            None,
            crate::durable::test_gate(dir.to_str().unwrap()),
            crate::notify::HubFaults::default(),
            Arc::new(crate::state::StateStore::new()),
        )
        .expect("spawn notifier");
        for job in jobs {
            tx.send(job).unwrap();
        }
        let read = || {
            open_direct(dir)
                .list_report_history(&ReportHistoryFilter::default())
                .unwrap()
        };
        if expect == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            return read().0;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let (rows, total) = read();
            if total >= expect || std::time::Instant::now() >= deadline {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    /// A webhook the send refuses at once (non-http scheme), so the attempt
    /// is recorded without any network.
    fn enable_webhook(dir: &std::path::Path, lifecycle: bool) {
        let storage = open_direct(dir);
        let mut global = storage.get_notify_global_config().unwrap();
        global.webhook_enabled = true;
        global.webhook_url = Some("ftp://receiver.invalid/hook".into());
        global.webhook_lifecycle = lifecycle;
        storage.upsert_notify_global_config(&global).unwrap();
    }

    /// decisions.md #29: `alarm.escalated` is a lifecycle event, sent only
    /// with the opt-in switch on.
    #[tokio::test]
    async fn the_escalation_webhook_follows_the_lifecycle_switch() {
        let off = tempfile::tempdir().unwrap();
        enable_webhook(off.path(), false);
        let rows = run_jobs(off.path(), vec![NotifyJob::Escalated(alarm_data("h1"))], 0).await;
        assert!(
            rows.iter().all(|r| r.event_type != "webhook_alarm"),
            "switch off: no escalation webhook, got {rows:?}"
        );

        let on = tempfile::tempdir().unwrap();
        enable_webhook(on.path(), true);
        let rows = run_jobs(on.path(), vec![NotifyJob::Escalated(alarm_data("h1"))], 1).await;
        assert!(
            rows.iter().any(|r| r.event_type == "webhook_alarm"
                && r.message
                    .as_deref()
                    .is_some_and(|m| m.contains("alarm.escalated"))),
            "switch on: the escalation webhook is attempted, got {rows:?}"
        );
    }

    /// decisions.md #31: a Hub without its own row e-mails the default
    /// recipients, for activation and escalation (what a new Hub row sends),
    /// not for clear.
    #[tokio::test]
    async fn a_hub_without_its_own_row_mails_the_default_recipients() {
        let dir = tempfile::tempdir().unwrap();
        open_direct(dir.path())
            .set_default_alarm_recipients(&["duty@example.com".to_string()])
            .unwrap();
        let rows = run_jobs(
            dir.path(),
            vec![
                NotifyJob::Activated(alarm_data("h1")),
                NotifyJob::Escalated(alarm_data("h1")),
                NotifyJob::Cleared {
                    data: alarm_data("h1"),
                    ack: None,
                },
            ],
            1,
        )
        .await;
        // Decision #19: the changes of one Hub within the batch window are one e-mail.
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].event_type, "alarm_batch");
        let message = rows[0].message.as_deref().unwrap_or_default();
        assert!(
            message.contains("(alarm)")
                && message.contains("(alarm_escalated)")
                && !message.contains("(alarm_clear)"),
            "{message}"
        );
        assert!(rows
            .iter()
            .all(|r| r.recipients == ["duty@example.com".to_string()]));
        assert!(has_alarm_recipients(
            None,
            &["duty@example.com".to_string()]
        ));
    }

    /// decisions.md #31: a Hub's own row overrides the defaults entirely.
    #[tokio::test]
    async fn a_hub_with_its_own_row_ignores_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        {
            let storage = open_direct(dir.path());
            storage
                .set_default_alarm_recipients(&["duty@example.com".to_string()])
                .unwrap();
            storage
                .upsert_device_report_config(&report_cfg(
                    true,
                    &["hub-owner@example.com"],
                    true,
                    false,
                    false,
                ))
                .unwrap();
        }
        let rows = run_jobs(dir.path(), vec![NotifyJob::Activated(alarm_data("h1"))], 1).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].recipients, ["hub-owner@example.com".to_string()]);

        // A row switched off stays the Hub's choice: no fallback.
        let dir = tempfile::tempdir().unwrap();
        {
            let storage = open_direct(dir.path());
            storage
                .set_default_alarm_recipients(&["duty@example.com".to_string()])
                .unwrap();
            storage
                .upsert_device_report_config(&report_cfg(
                    false,
                    &["hub-owner@example.com"],
                    true,
                    false,
                    false,
                ))
                .unwrap();
        }
        let rows = run_jobs(dir.path(), vec![NotifyJob::Activated(alarm_data("h1"))], 0).await;
        assert!(rows.is_empty(), "{rows:?}");
        let off = report_cfg(false, &["hub-owner@example.com"], true, false, false);
        assert!(!has_alarm_recipients(
            Some(&off),
            &["duty@example.com".to_string()]
        ));
    }

    /// decisions.md #31: neither defaults nor a row — nothing is sent and the
    /// Hub is reported as unreached.
    #[tokio::test]
    async fn no_defaults_and_no_row_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let rows = run_jobs(dir.path(), vec![NotifyJob::Activated(alarm_data("h1"))], 0).await;
        assert!(rows.is_empty(), "{rows:?}");
        assert!(!has_alarm_recipients(None, &[]));
    }

    #[tokio::test]
    async fn system_alert_onset_and_rearm_record_one_row_each() {
        let dir = tempfile::tempdir().unwrap();
        {
            let storage = open_direct(dir.path());
            let mut cfg = storage.get_system_alert_config().unwrap();
            cfg.recipients = vec!["ops@example.com".into()];
            storage.upsert_system_alert_config(&cfg).unwrap();
        }
        let faults = crate::notify::HubFaults::default();
        let tx = spawn_notifier(
            dir.path().to_str().unwrap(),
            None,
            crate::durable::test_gate(dir.path().to_str().unwrap()),
            faults.clone(),
            Arc::new(crate::state::StateStore::new()),
        )
        .expect("spawn notifier");

        let low_battery = SystemSnapshot {
            battery_pct: Some(5.0),
            online: true,
            has_internet: true,
            ..Default::default()
        };
        let healthy = SystemSnapshot {
            battery_pct: Some(80.0),
            online: true,
            has_internet: true,
            ..Default::default()
        };

        tx.send(NotifyJob::SystemSnapshot {
            hostname: "h1".into(),
            snapshot: low_battery.clone(),
        })
        .unwrap();
        tx.send(NotifyJob::SystemSnapshot {
            hostname: "h1".into(),
            snapshot: low_battery.clone(),
        })
        .unwrap(); // debounced
        tx.send(NotifyJob::SystemSnapshot {
            hostname: "h1".into(),
            snapshot: healthy,
        })
        .unwrap(); // re-arms
        tx.send(NotifyJob::SystemSnapshot {
            hostname: "h1".into(),
            snapshot: low_battery,
        })
        .unwrap(); // fires again

        // Poll rather than a fixed sleep: this is the one test in the module that
        // asserts a positive row count, so a notifier still working through its queue
        // under a saturated test runner read as a failure (150 ms was measured flaky).
        // The zero-count tests keep their sleeps — for them a short wait is a false
        // pass at worst, not a flake.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let (rows, total) = loop {
            let storage = open_direct(dir.path());
            let (rows, total) = storage
                .list_report_history(&ReportHistoryFilter::default())
                .unwrap();
            if total >= 2 || std::time::Instant::now() >= deadline {
                break (rows, total);
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        };
        drop(tx);
        assert_eq!(
            total, 2,
            "onset + re-arm onset, debounce/healthy produce no row"
        );
        assert!(rows
            .iter()
            .all(|r| r.event_type == "battery_low" && r.source == "system_alert"));
        // The last snapshot left the battery low, and the Hubs page reads that.
        assert!(faults.any("h1"));
        assert!(!faults.any("h2"));
    }

    #[tokio::test]
    async fn acknowledged_notify_job_uses_ack_gate() {
        let dir = tempfile::tempdir().unwrap();
        {
            let storage = open_direct(dir.path());
            storage
                .upsert_device_report_config(&report_cfg(
                    true,
                    &["ops@example.com"],
                    true,
                    false,
                    true,
                ))
                .unwrap();
            storage
                .create_user(&portal_storage::NewUser {
                    username: "alice".into(),
                    password_hash: "hash".into(),
                    full_name: Some("Alice Nováková".into()),
                    job_title: None,
                    user_role: "viewer".into(),
                    is_superuser: false,
                })
                .unwrap();
        }
        let tx = spawn_notifier(
            dir.path().to_str().unwrap(),
            None,
            crate::durable::test_gate(dir.path().to_str().unwrap()),
            crate::notify::HubFaults::default(),
            Arc::new(crate::state::StateStore::new()),
        )
        .expect("spawn notifier");

        let alarm = AlarmEvent {
            id: "a1".into(),
            hostname: "h1".into(),
            sensor_id: "line-0".into(),
            sensor_name: Some("Fridge".into()),
            sensor_location: None,
            alarm_type: "HIGH".into(),
            severity: "warning".into(),
            from_state: None,
            to_state: None,
            message: None,
            value: Some(9.0),
            threshold: Some(8.0),
            triggered_at: "2026-08-18T00:00:00Z".into(),
            cleared_at: None,
            acknowledged_at: Some("2026-08-18T00:05:00Z".into()),
            acknowledged_by_user_id: Some(1),
            acknowledged_by_username: Some("alice".into()),
            native_key: None,
            created_at: "2026-08-18T00:00:00Z".into(),
            max_severity: "warning".into(),
            end_reason: None,
        };
        tx.send(NotifyJob::Acknowledged {
            alarm,
            user: "alice".into(),
            reason: "checked".into(),
        })
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(tx);

        let storage = open_direct(dir.path());
        let (rows, total) = storage
            .list_report_history(&ReportHistoryFilter::default())
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].event_type, "alarm_ack");
        // The reader gets the person, the username stays beside it.
        assert!(
            rows[0]
                .subject
                .as_deref()
                .is_some_and(|s| s.starts_with("[POTVRDIL Alice Nováková (alice)] ")),
            "{:?}",
            rows[0].subject
        );
    }
}
