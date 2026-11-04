//! Beacon archive history — the portal half of plan/portal/beacon_history.md
//! (B6b, phase P2: §4 and §7).
//!
//! The Hub runs the download: `control/ble` dials the Beacon, walks its
//! archive and owns the retries (§2.4). The portal asks for a window with a
//! signed `beacon-history {mac, from, to, job}` (§2.1) and records what comes
//! back on `{prefix}/{hostname}/eye/history` (§2.2-2.3):
//!
//! - the job's progress, into `beacon_history_jobs` (V29), written by the
//!   persist thread like every other ingest row;
//! - the archive's rows, into `eye_readings` as `event_type = 'recording'`.
//!   They never reach an alarm engine (§4.4, Q4): the `eye/history` arm in
//!   `persist::handle` simply does not call one.
//!
//! This module holds the wire shape, the job service the routes and the
//! automatic path use, and the pure pieces both sides share (the
//! cross-check of §7 Q6 layer 3, the gap and auto-window arithmetic).
//!
//! The History tab is the Node's (§7.1): the same
//! `partials/backfill_jobs.html`, fed [`BeaconJobCtx`] rows that carry the
//! Node's field names, and re-rendered over SSE on every
//! [`BeaconHistoryNudge`].

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use portal_storage::{
    BeaconHistoryJob, NewAuditEntry, NewBeaconHistoryJob, SpillGate, Storage, StorageError,
    JOB_FAILED, THRESHOLD_SCOPE_EYE,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::backfill::{format_duration, format_ts_minutes, missing_samples, GapCtx};
use crate::keys::KeyService;
use crate::notify::offline::{beacon_interval_s, DeviceScope, Transition};
use crate::offline::HubTransition;
use crate::relay::CommandRelay;
use crate::state::StateStore;

/// The registry verb (fiber-protocol `beacon-history`, `beacon.manage` at
/// `read`).
pub const COMMAND: &str = "beacon-history";
/// `trigger` of a job an operator asked for — the Node's value (§7.1).
pub const MANUAL_TRIGGER: &str = "manual";
/// `trigger` of the job a return from offline starts (§4.2, §7.1).
pub const AUTO_TRIGGER: &str = "auto-on-reconnect";
/// The automatic window's cap (§7 Q3).
pub const AUTO_WINDOW_CAP_S: i64 = 24 * 3600;
/// How long the Hub may say nothing about a job in flight before the portal
/// fails it itself (§4.1: a Hub restart lost its queue).
pub const SILENCE_LIMIT_S: i64 = 30 * 60;
/// The same for a job the Hub is retrying (`retry >= 1`): its retry ladder
/// waits up to 60 min before the third attempt (beacon_history.md §8), so
/// silence is judged only past that wait plus [`SILENCE_LIMIT_S`].
pub const RETRY_SILENCE_LIMIT_S: i64 = 60 * 60 + SILENCE_LIMIT_S;
/// How far back the History tab looks for gaps (§4.3 item 3).
pub const GAP_LOOKBACK_S: i64 = 7 * 86_400;
/// At most this many gap rows, newest first — the Node's cap.
pub const MAX_RENDERED_GAPS: usize = 16;
/// §7 Q6 layer 3: an archive point and a live reading closer than half an
/// archive interval must agree within this many °C.
pub const CROSSCHECK_TOLERANCE_C: f64 = 0.5;
/// The archive cadence assumed when nothing says otherwise: the Hub's
/// `eye.recording_interval_min` default (§3).
pub const DEFAULT_RECORDING_INTERVAL_S: i64 = 15 * 60;
/// `reason` the portal writes when it fails a job the Hub went silent on.
pub const REASON_SILENT: &str = "silent";
/// `reason` the portal writes when the request never reached the Hub.
pub const REASON_SEND_FAILED: &str = "send_failed";
/// Who the portal's own audit rows about archive downloads name.
pub const SYSTEM_ACTOR: &str = "system:beacon-history";

const NUDGE_CHANNEL_CAPACITY: usize = 256;
const REAPER_TICK: Duration = Duration::from_secs(60);

/// One `eye/history` message (§2.2 state, §2.3 records). Parsed here rather
/// than in fiber-protocol: the portal reads it, and every field but the three
/// that identify it is optional so a Hub a step ahead or behind still lands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BeaconHistoryMsg {
    pub job: String,
    pub mac: String,
    /// `requested` | `receiving` | `done` | `failed` | `records`.
    pub state: String,
    #[serde(default)]
    pub received: Option<i64>,
    #[serde(default)]
    pub expected: Option<i64>,
    #[serde(default)]
    pub from: Option<i64>,
    #[serde(default)]
    pub to: Option<i64>,
    #[serde(default)]
    pub retry: Option<i64>,
    #[serde(default)]
    pub reason: Option<String>,
    /// §7 Q6 layers 1-2, on `done`.
    #[serde(default)]
    pub sessions: Option<i64>,
    #[serde(default)]
    pub rejected_chunks: Option<i64>,
    #[serde(default)]
    pub future_dropped: Option<i64>,
    #[serde(default)]
    pub crc_failed: Option<i64>,
    /// `records` only.
    #[serde(default)]
    pub seq: Option<i64>,
    #[serde(default)]
    pub interval_s: Option<i64>,
    /// `[[ts, temperature_c], …]`, at most 240 per message.
    #[serde(default)]
    pub records: Vec<(i64, Option<f64>)>,
}

/// The state that carries rows rather than progress (§2.3).
pub const STATE_RECORDS: &str = "records";

/// "This Beacon's History tab is stale — re-render it." Same coarse signal
/// as [`crate::backfill::BackfillNudge`]: no job data, the SSE layer
/// re-reads.
#[derive(Debug, Clone)]
pub struct BeaconHistoryNudge {
    pub hostname: String,
    /// Uppercase.
    pub mac: String,
}

/// The History tab table's element id and SSE event name. A MAC's colons
/// are not valid in a CSS selector, so they become dashes (as
/// `beacon_detail.html`'s `mac_id`).
pub fn swap_id(mac: &str) -> String {
    format!("beacon-history-{}", mac.to_uppercase().replace(':', "-"))
}

/// The base path the table's buttons post to: `{post_url}` requests a window,
/// `{post_url}/{job}/cancel` cancels one.
pub fn post_url(mac: &str) -> String {
    format!("/beacons/{}/history", mac.to_uppercase())
}

/// A fresh nudge channel. Created in `main` before either end exists: the
/// persist thread sends on it, the service hands out its receivers.
pub fn nudge_channel() -> broadcast::Sender<BeaconHistoryNudge> {
    broadcast::channel(NUDGE_CHANNEL_CAPACITY).0
}

/// `bh-` + a ULID (48-bit millisecond time, 80 random bits, Crockford
/// base32): sorts by creation and needs no coordination (§4.1).
pub fn new_job_id() -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
        & ((1u128 << 48) - 1);
    let mut random = [0u8; 10];
    rand::thread_rng().fill_bytes(&mut random);
    let mut value = millis << 80;
    for (i, byte) in random.iter().enumerate() {
        value |= u128::from(*byte) << (72 - 8 * i);
    }
    let mut out = String::with_capacity(29);
    out.push_str("bh-");
    for i in (0..26).rev() {
        out.push(ALPHABET[((value >> (5 * i)) & 0x1f) as usize] as char);
    }
    out
}

/// §7 Q6 layer 3. `points` is one Beacon's readings ordered by `ts`, every
/// event type; each `recording` point with a temperature is compared with the
/// nearest non-archive reading within `half_window_s`. Returns `(compared,
/// disagreed)`: how many archive points had a live neighbour, and how many of
/// those differ by more than [`CROSSCHECK_TOLERANCE_C`].
pub fn cross_check(points: &[(i64, Option<f64>, String)], half_window_s: i64) -> (i64, i64) {
    let live: Vec<(i64, f64)> = points
        .iter()
        .filter(|(_, _, kind)| kind != "recording")
        .filter_map(|(ts, t, _)| t.map(|t| (*ts, t)))
        .collect();
    let mut compared = 0;
    let mut disagreed = 0;
    for (ts, temp, kind) in points {
        if kind != "recording" {
            continue;
        }
        let Some(temp) = temp else { continue };
        // `live` is sorted, so the nearest neighbour sits at the partition
        // point or just before it.
        let at = live.partition_point(|(t, _)| t < ts);
        let nearest = [at.checked_sub(1), Some(at)]
            .into_iter()
            .flatten()
            .filter_map(|i| live.get(i))
            .min_by_key(|(t, _)| (t - ts).abs());
        let Some((live_ts, live_temp)) = nearest else {
            continue;
        };
        if (live_ts - ts).abs() > half_window_s {
            continue;
        }
        compared += 1;
        if (live_temp - temp).abs() > CROSSCHECK_TOLERANCE_C {
            disagreed += 1;
        }
    }
    (compared, disagreed)
}

/// The archive cadence a set of `recording` timestamps shows: the smallest
/// positive step between two of them, or `None` with fewer than two.
pub fn recording_interval(points: &[(i64, Option<f64>, String)]) -> Option<i64> {
    let ts: Vec<i64> = points
        .iter()
        .filter(|(_, _, kind)| kind == "recording")
        .map(|(ts, _, _)| *ts)
        .collect();
    ts.windows(2).map(|w| w[1] - w[0]).filter(|d| *d > 0).min()
}

/// Holes in a sorted list of reading timestamps: consecutive readings more
/// than `2 x interval_s` apart (the Node's rule, devices.backend.md §6).
pub fn detect_gaps(timestamps: &[i64], interval_s: i64) -> Vec<(i64, i64)> {
    let interval = interval_s.max(1);
    timestamps
        .windows(2)
        .filter(|w| w[1] - w[0] > 2 * interval)
        .map(|w| (w[0], w[1]))
        .collect()
}

/// The automatic job's window after a return (§4.2): from the oldest hole of
/// the last `cap_s` to the newest one's end, keeping the newest part when the
/// span is wider than the cap. `timestamps` are the Beacon's readings since
/// `now - cap_s - 2 x interval_s`; `now` closes the list, because the reading
/// that ended the outage may not be stored yet (its storage window is still
/// open on the Hub). An empty list is one hole across the whole window. `None` = nothing missing.
pub fn auto_window(
    timestamps: &[i64],
    interval_s: i64,
    now: i64,
    cap_s: i64,
) -> Option<(i64, i64)> {
    let start = now - cap_s;
    let mut edges: Vec<i64> = Vec::with_capacity(timestamps.len() + 2);
    if timestamps.first().is_none_or(|first| *first > start) {
        edges.push(start);
    }
    edges.extend(timestamps.iter().copied().filter(|t| *t <= now));
    edges.push(now);
    let gaps = detect_gaps(&edges, interval_s);
    let from = gaps.first()?.0.max(start);
    let to = gaps.last()?.1;
    (to > from).then_some((from, to))
}

/// `partials/backfill_jobs.html`'s row, in the Node's field names (§7.1), so
/// one template draws both. `label` is the short id the `#` column shows; the
/// full one is `job_id`, which the Cancel URL carries.
#[derive(Debug, Clone, Serialize)]
pub struct BeaconJobCtx {
    pub job_id: String,
    pub label: String,
    pub status: String,
    pub time_from: String,
    pub time_to: String,
    pub trigger: String,
    pub samples_received: i64,
    /// At most this many archive records fit the window: the window over the
    /// Beacon's recording interval, rounded up, as the Hub counts its
    /// `expected`. See [`job_ctx`] for which number is shown.
    pub samples_expected: Option<i64>,
    /// The recording interval `samples_expected` was counted at, in minutes,
    /// for the line under it; `None` when it is not known.
    pub samples_interval_min: Option<i64>,
    /// `samples_interval_min` is the default (§3), not one the Beacon reported.
    pub samples_interval_assumed: bool,
    pub retry_count: i64,
    pub device_refused: bool,
    pub error_message: Option<String>,
    pub refusal: Option<&'static str>,
    /// The Hub's or the portal's failure reason, worded by the template
    /// (`monitoring.beacon_history.reason.*`).
    pub reason: Option<String>,
    pub created_at: String,
    /// The window as unix seconds (0 while unknown), for the coverage bar.
    pub from_unix: i64,
    pub to_unix: i64,
}

/// Maps a stored job onto the template row.
///
/// `archive_interval_s` is the Beacon's own recording interval as it last
/// reported it (`ble_config.logging_interval_min`). The expected count is
/// the most the window can hold: the window over that interval, rounded down,
/// plus one, because both ends are inclusive — the Hub's `expected_records`,
/// so the number does not change when the Hub
/// reports. The Hub's own `expected` wins once it has sent one (it counted
/// with the interval the Beacon reported at the download); before that the
/// Portal counts the same way, at the default interval while the Beacon's
/// is unknown, and says it assumed it. It was the Portal's store interval and
/// a floor division before (hardware test 2026-10-09).
pub fn job_ctx(job: &BeaconHistoryJob, archive_interval_s: Option<i64>) -> BeaconJobCtx {
    let window = match (job.from_ts, job.to_ts) {
        (Some(from), Some(to)) if to > from => Some(to - from),
        _ => None,
    };
    let count = |interval_s: i64| window.map(|w| w / interval_s + 1);
    let reported = archive_interval_s.filter(|s| *s > 0);
    let (samples_expected, samples_interval_min, samples_interval_assumed) = match job.expected {
        // The Hub's count: name the interval only when it is the one it counted with.
        Some(expected) => (
            Some(expected),
            reported
                .filter(|s| count(*s) == Some(expected))
                .map(|s| s / 60),
            false,
        ),
        None => {
            let interval_s = reported.unwrap_or(DEFAULT_RECORDING_INTERVAL_S);
            match count(interval_s) {
                Some(n) => (Some(n), Some(interval_s / 60), reported.is_none()),
                None => (None, None, false),
            }
        }
    };
    let reason = job
        .reason
        .clone()
        .filter(|_| job.state == JOB_FAILED || job.retry > 0);
    BeaconJobCtx {
        job_id: job.id.clone(),
        label: short_label(&job.id),
        status: job.state.clone(),
        time_from: job
            .from_ts
            .map(format_ts_minutes)
            .unwrap_or_else(|| "…".into()),
        time_to: job
            .to_ts
            .map(format_ts_minutes)
            .unwrap_or_else(|| "…".into()),
        trigger: job.trigger.clone(),
        samples_received: job.received,
        samples_expected,
        samples_interval_min,
        samples_interval_assumed,
        retry_count: job.retry,
        device_refused: false,
        error_message: None,
        refusal: None,
        reason,
        created_at: format_ts_minutes(job.created_at),
        from_unix: job.from_ts.unwrap_or(0),
        to_unix: job.to_ts.unwrap_or(0),
    }
}

/// The last six characters of an id — the random tail of a ULID, enough to
/// tell a page's jobs apart.
fn short_label(id: &str) -> String {
    let chars: Vec<char> = id.chars().collect();
    chars[chars.len().saturating_sub(6)..].iter().collect()
}

/// Why a request was refused before anything was created or sent.
#[derive(Debug, thiserror::Error)]
pub enum BeaconHistoryError {
    /// §4.1: one job per Beacon in flight. Carries the running one.
    #[error("a history download for this Beacon is already running")]
    InFlight(Box<BeaconHistoryJob>),
    #[error("window ends before it starts")]
    EmptyWindow,
    #[error("window ends in the future")]
    FutureWindow,
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// The job service: creates and settles the portal's side of a job, the
/// automatic request after a return, and the silence reaper. Cheap to clone.
#[derive(Clone)]
pub struct BeaconHistoryService {
    db: crate::db::Db,
    relay: CommandRelay,
    /// For the automatic path's `portal-service-beacon` signer. `None` in a
    /// bare test service, where the automatic request is not sent.
    keys: Option<KeyService>,
    spill: Arc<SpillGate>,
    beacons: crate::config::BeaconsConfig,
    nudge_tx: broadcast::Sender<BeaconHistoryNudge>,
}

impl BeaconHistoryService {
    /// Opens its own connection on `<data_dir>/portal.db`, as every service.
    pub fn open(
        data_dir: &str,
        db_key: Option<&str>,
        relay: CommandRelay,
        spill: Arc<SpillGate>,
        beacons: crate::config::BeaconsConfig,
        nudge_tx: broadcast::Sender<BeaconHistoryNudge>,
    ) -> Result<Self, StorageError> {
        std::fs::create_dir_all(data_dir)?;
        let storage = Storage::open(&Path::new(data_dir).join("portal.db"), db_key)?;
        Ok(Self {
            db: crate::db::Db::from_storage(storage, "beacon-history"),
            relay,
            keys: None,
            spill,
            beacons,
            nudge_tx,
        })
    }

    pub fn with_keys(mut self, keys: KeyService) -> Self {
        self.keys = Some(keys);
        self
    }

    /// The SSE layer's subscription point.
    pub fn subscribe_nudges(&self) -> broadcast::Receiver<BeaconHistoryNudge> {
        self.nudge_tx.subscribe()
    }

    pub fn nudge(&self, hostname: &str, mac: &str) {
        let _ = self.nudge_tx.send(BeaconHistoryNudge {
            hostname: hostname.to_string(),
            mac: mac.to_string(),
        });
    }

    /// Spawns the transition listener (§4.2) and the silence reaper (§4.1).
    pub fn spawn_tasks(
        &self,
        store: Arc<StateStore>,
        transitions: Option<broadcast::Receiver<HubTransition>>,
    ) {
        if let Some(mut rx) = transitions {
            let svc = self.clone();
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(t) => svc.handle_transition(&store, t).await,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // A missed return costs one automatic download; the
                            // gap list still offers the window by hand.
                            tracing::warn!(missed = n, "beacon history missed offline transitions");
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                }
            });
        }
        let reaper = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REAPER_TICK);
            loop {
                tick.tick().await;
                reaper.reap_silent(now_unix()).await;
            }
        });
    }

    /// The Beacon store interval, seconds — the cadence its gaps are measured
    /// against (the offline detector's number, §4.3 item 3). One fleet value
    /// since the per-Beacon override was retired
    /// (`plan/control/storage_intervals.md` S4); the arguments stay so the
    /// callers do not change if a per-Beacon value ever returns.
    pub async fn store_interval_s(&self, _hostname: &str, _mac: &str) -> i64 {
        beacon_interval_s(&self.beacons)
    }

    /// `device_meta.backfill_auto` for a Beacon (§4.2). No row = on (V9's
    /// default); an unreadable table also reads on, as for the Node.
    pub async fn auto_enabled(&self, hostname: &str, mac: &str) -> bool {
        let host = hostname.to_string();
        let entity = mac.to_string();
        match self
            .db
            .run(move |s| s.get_device_meta(&host, THRESHOLD_SCOPE_EYE, &entity))
            .await
        {
            Ok(Some(meta)) => meta.backfill_auto,
            Ok(None) => true,
            Err(err) => {
                tracing::warn!(%err, hostname, mac, "device_meta unreadable; assuming backfill_auto");
                true
            }
        }
    }

    /// Creates a `pending` job, refusing a second one in flight (§4.1) and a
    /// window that is empty or reaches into the future. `from`/`to` `None`
    /// is the resume request (§2.1). Any width is allowed (§7 Q3).
    pub async fn create(
        &self,
        hostname: &str,
        mac: &str,
        from: Option<i64>,
        to: Option<i64>,
        trigger: &str,
        created_by: Option<String>,
    ) -> Result<BeaconHistoryJob, BeaconHistoryError> {
        if let (Some(from), Some(to)) = (from, to) {
            if to <= from {
                return Err(BeaconHistoryError::EmptyWindow);
            }
        }
        // A minute of slack for the clock the form was typed against.
        if to.is_some_and(|to| to > now_unix() + 60) {
            return Err(BeaconHistoryError::FutureWindow);
        }
        let mac = mac.to_uppercase();
        let job = NewBeaconHistoryJob {
            id: new_job_id(),
            hostname: hostname.to_string(),
            mac: mac.clone(),
            from_ts: from,
            to_ts: to,
            trigger: trigger.to_string(),
            created_by,
        };
        let created = self
            .db
            .write(move |s| {
                if let Some(running) = s.active_beacon_history_job(&job.mac)? {
                    return Ok(Err(running));
                }
                s.create_beacon_history_job(&job).map(Ok)
            })
            .await?;
        match created {
            Ok(job) => {
                self.nudge(hostname, &mac);
                Ok(job)
            }
            Err(running) => Err(BeaconHistoryError::InFlight(Box::new(running))),
        }
    }

    /// The request's params (§2.1).
    pub fn params(job: &BeaconHistoryJob) -> serde_json::Value {
        let mut params = serde_json::json!({ "mac": job.mac, "job": job.id });
        if let Some(from) = job.from_ts {
            params["from"] = from.into();
        }
        if let Some(to) = job.to_ts {
            params["to"] = to.into();
        }
        params
    }

    /// What the relay said about the request, applied to a job still
    /// `pending`: acknowledged → `requested`; refused by the Hub → `failed`.
    /// No answer leaves it `pending` (cancellable), and the reaper fails it
    /// if the Hub stays silent.
    pub async fn settle(&self, job: &BeaconHistoryJob, status: &str) {
        let (state, reason) = match status {
            "success" => ("requested", None),
            "error" => (JOB_FAILED, Some(REASON_SEND_FAILED)),
            _ => return,
        };
        let id = job.id.clone();
        if let Err(err) = self
            .db
            .write(move |s| s.settle_pending_beacon_history_job(&id, state, reason))
            .await
        {
            tracing::warn!(%err, job = %job.id, "beacon history job could not be settled");
        }
        self.nudge(&job.hostname, &job.mac);
    }

    /// Forgets a job that was refused before anything was signed.
    pub async fn discard(&self, job: &BeaconHistoryJob) {
        let id = job.id.clone();
        if let Err(err) = self
            .db
            .write(move |s| s.delete_beacon_history_job(&id))
            .await
        {
            tracing::warn!(%err, job = %job.id, "unsent beacon history job could not be removed");
        }
        self.nudge(&job.hostname, &job.mac);
    }

    /// Cancels one of this Beacon's jobs while it is `pending`.
    pub async fn cancel(&self, hostname: &str, mac: &str, id: &str) -> Result<bool, StorageError> {
        let mac = mac.to_uppercase();
        let (m, i) = (mac.clone(), id.to_string());
        let cancelled = self
            .db
            .write(move |s| s.cancel_beacon_history_job(&m, &i))
            .await?;
        self.nudge(hostname, &mac);
        Ok(cancelled)
    }

    /// The jobs table's rows, newest first. Degrades to empty.
    pub async fn jobs_ctx(
        &self,
        store: &StateStore,
        hostname: &str,
        mac: &str,
    ) -> Vec<BeaconJobCtx> {
        let interval = store
            .beacon_logging_interval_min(hostname, mac)
            .map(|min| min * 60);
        let m = mac.to_uppercase();
        match self
            .db
            .run(move |s| s.list_beacon_history_jobs(&m, None))
            .await
        {
            Ok(jobs) => jobs.iter().map(|j| job_ctx(j, interval)).collect(),
            Err(err) => {
                tracing::warn!(%err, mac, "beacon history jobs unreadable; rendering none");
                Vec::new()
            }
        }
    }

    /// The gap rows (§4.3 item 3): the last 7 days, holes longer than twice
    /// the store interval, newest first, at most [`MAX_RENDERED_GAPS`].
    pub async fn gaps_ctx(&self, hostname: &str, mac: &str) -> Vec<GapCtx> {
        let interval = self.store_interval_s(hostname, mac).await;
        let now = now_unix();
        let m = mac.to_uppercase();
        let timestamps = match self
            .db
            .run(move |s| s.eye_reading_timestamps(&m, now - GAP_LOOKBACK_S, now + 1))
            .await
        {
            Ok(ts) => ts,
            Err(err) => {
                tracing::warn!(%err, mac, "beacon gap detection failed; rendering none");
                return Vec::new();
            }
        };
        detect_gaps(&timestamps, interval)
            .into_iter()
            .rev()
            .take(MAX_RENDERED_GAPS)
            .map(|(from, to)| GapCtx {
                time_from: format_ts_minutes(from),
                time_to: format_ts_minutes(to),
                from_unix: from,
                to_unix: to,
                duration: format_duration(to - from),
                samples_expected: missing_samples(from, to, interval),
                unavailable: None,
            })
            .collect()
    }

    /// One liveness edge. Only a Beacon's return matters here: it starts the
    /// automatic download of what the outage cost (§4.2).
    pub async fn handle_transition(&self, store: &StateStore, t: HubTransition) {
        if t.transition.scope != DeviceScope::Beacon
            || t.transition.transition != Transition::CameBack
        {
            return;
        }
        let hostname = store
            .snapshot_device(&t.hostname)
            .map(|d| d.hostname.clone())
            .unwrap_or(t.hostname);
        let mac = t.transition.entity.to_uppercase();
        // Decision #42: the job is the registry's, the GATT readout its home Hub's.
        let gatt_host = store
            .snapshot()
            .home_hub_of_beacon(&mac)
            .map(|d| d.hostname.clone())
            .unwrap_or_else(|| hostname.clone());
        self.auto_request(&hostname, &gatt_host, &mac, now_unix())
            .await;
    }

    /// The automatic request after a return: behind the paired-Hub rule
    /// (decision #18) and `backfill_auto`, never next to a job in flight,
    /// for the holes of the last 24 h. Returns the job it sent, if any.
    ///
    /// `hostname` keys the job (the registry); `gatt_host` is the Hub that reads the Beacon —
    /// its home Hub in a cluster (decision #42), the same Hub outside one.
    pub async fn auto_request(
        &self,
        hostname: &str,
        gatt_host: &str,
        mac: &str,
        now: i64,
    ) -> Option<BeaconHistoryJob> {
        let Some(keys) = &self.keys else {
            tracing::debug!(
                hostname,
                mac,
                "beacon history: no key service — no automatic request"
            );
            return None;
        };
        if !hub_paired(keys, gatt_host).await {
            tracing::debug!(
                hostname,
                mac,
                "beacon history: Hub not paired — no automatic request"
            );
            return None;
        }
        if !self.auto_enabled(hostname, mac).await {
            return None;
        }
        let interval = self.store_interval_s(hostname, mac).await;
        let m = mac.to_string();
        let since = now - AUTO_WINDOW_CAP_S - 2 * interval;
        let timestamps = self
            .db
            .run(move |s| s.eye_reading_timestamps(&m, since, now + 1))
            .await
            .ok()?;
        let (from, to) = auto_window(&timestamps, interval, now, AUTO_WINDOW_CAP_S)?;

        let job = match self
            .create(hostname, mac, Some(from), Some(to), AUTO_TRIGGER, None)
            .await
        {
            Ok(job) => job,
            Err(BeaconHistoryError::InFlight(running)) => {
                // One download per Beacon at a time (§4.1). The hole this
                // outage left stays in the gap list with its own button.
                tracing::info!(mac, running = %running.id, "beacon history: a job is in flight; no automatic request");
                return None;
            }
            Err(err) => {
                tracing::warn!(%err, mac, "beacon history: automatic job not created");
                return None;
            }
        };

        let key = match keys.ensure_beacon_service_key(gatt_host).await {
            Ok(key) => key,
            Err(err) => {
                tracing::warn!(%err, hostname, "beacon history: no service signing key");
                self.discard(&job).await;
                return None;
            }
        };
        let outcome = self
            .relay
            .dispatch(
                crate::keys::PORTAL_BEACON_SERVICE_SIGNER,
                &key,
                gatt_host,
                COMMAND,
                Self::params(&job),
                Some(format!("beacon history job {}", job.id)),
            )
            .await;
        if outcome.request_id.is_none() {
            tracing::warn!(job = %job.id, message = ?outcome.message, "beacon-history refused before signing");
            self.discard(&job).await;
            return None;
        }
        if outcome.status == "error" {
            self.settle(&job, "error").await;
        }
        self.audit_system(
            "beacon_history_auto",
            mac,
            serde_json::json!({
                "job": job.id,
                "hostname": hostname,
                "from": from,
                "to": to,
                "status": outcome.status,
            }),
            outcome.status != "error",
        )
        .await;
        Some(job)
    }

    /// Fails every job the Hub has been silent about for
    /// [`SILENCE_LIMIT_S`], or [`RETRY_SILENCE_LIMIT_S`] while it retries
    /// (§4.1, §8).
    pub async fn reap_silent(&self, now: i64) {
        let cutoff = now - SILENCE_LIMIT_S;
        let retry_cutoff = now - RETRY_SILENCE_LIMIT_S;
        match self
            .db
            .write(move |s| s.fail_silent_beacon_history_jobs(cutoff, retry_cutoff, REASON_SILENT))
            .await
        {
            Ok(failed) => {
                for job in failed {
                    tracing::info!(job = %job.id, mac = %job.mac, "beacon history job failed: Hub silent");
                    self.nudge(&job.hostname, &job.mac);
                }
            }
            Err(err) => tracing::warn!(%err, "beacon history silence sweep failed"),
        }
    }

    async fn audit_system(
        &self,
        event_type: &str,
        target: &str,
        details: serde_json::Value,
        success: bool,
    ) {
        let entry = NewAuditEntry {
            event_type: event_type.to_string(),
            username: Some(SYSTEM_ACTOR.to_string()),
            user_id: None,
            ip: None,
            target: Some(target.to_string()),
            details: serde_json::to_string(&details).ok(),
            success,
        };
        let spill = Arc::clone(&self.spill);
        if let Err(err) = self
            .db
            .run(move |s| crate::durable::insert_audit(&spill, s, &entry))
            .await
        {
            tracing::error!(%err, event_type, "beacon history audit entry could not be written or spilled");
        }
    }
}

/// Decision #18, as `BackfillService::hub_paired`: history only from a paired
/// Hub, looked up under the spellings a Hub arrives under.
async fn hub_paired(keys: &KeyService, hostname: &str) -> bool {
    for spelling in [
        hostname.to_string(),
        hostname.to_ascii_lowercase(),
        hostname.to_ascii_uppercase(),
    ] {
        if matches!(keys.device_identity(&spelling).await, Ok(Some(_))) {
            return true;
        }
    }
    false
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
impl BeaconHistoryService {
    /// A service on a fresh db that never really publishes.
    pub fn test_instance(data_dir: &str) -> Self {
        Self::open(
            data_dir,
            None,
            crate::relay::CommandRelay::test_instance(),
            crate::durable::test_gate(data_dir),
            crate::config::BeaconsConfig::default(),
            nudge_channel(),
        )
        .expect("open beacon history service")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portal_storage::JOB_DONE;

    fn rec(ts: i64, t: f64) -> (i64, Option<f64>, String) {
        (ts, Some(t), "recording".into())
    }
    fn live(ts: i64, t: f64) -> (i64, Option<f64>, String) {
        (ts, Some(t), "live".into())
    }

    #[test]
    fn job_ids_are_ulids_that_sort_by_creation() {
        let a = new_job_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_job_id();
        assert_eq!(a.len(), 29);
        assert!(a.starts_with("bh-"));
        assert!(a < b, "{a} sorts before {b}");
        assert_ne!(new_job_id(), new_job_id());
    }

    #[test]
    fn the_cross_check_counts_only_points_with_a_live_neighbour() {
        let points = vec![
            live(0, 4.0),
            rec(100, 4.2), // 100 s from a live reading, agrees
            rec(900, 9.0), // 100 s from the next live one, disagrees
            live(1000, 4.0),
            rec(5000, 4.0), // nothing live within 450 s
        ];
        assert_eq!(cross_check(&points, 450), (2, 1));
        assert_eq!(recording_interval(&points), Some(800));
    }

    #[test]
    fn a_tolerance_of_half_a_degree_is_inclusive() {
        let points = vec![live(0, 4.0), rec(10, 4.5)];
        assert_eq!(cross_check(&points, 450), (1, 0));
    }

    #[test]
    fn the_automatic_window_spans_the_holes_and_stops_at_24_hours() {
        let now = 100_000;
        let cap = AUTO_WINDOW_CAP_S;
        // Readings every 15 min up to an outage at now-3h, nothing since.
        let ts: Vec<i64> = (0..40)
            .map(|i| now - cap + i * 900)
            .filter(|t| *t < now - 3 * 3600)
            .collect();
        let last = *ts.last().unwrap();
        assert_eq!(auto_window(&ts, 900, now, cap), Some((last, now)));
        // No readings at all: the whole capped window.
        assert_eq!(auto_window(&[], 900, now, cap), Some((now - cap, now)));
        // Readings right up to now: nothing to fetch.
        let full: Vec<i64> = (0..=96).map(|i| now - cap + i * 900).collect();
        assert_eq!(auto_window(&full, 900, now, cap), None);
    }

    #[test]
    fn job_rows_speak_the_node_partials_field_names() {
        let job = BeaconHistoryJob {
            id: "bh-01J0000000000000000ABCDEF".into(),
            hostname: "hub".into(),
            mac: "AA:BB:CC:DD:EE:01".into(),
            from_ts: Some(0),
            to_ts: Some(9000),
            trigger: AUTO_TRIGGER.into(),
            state: JOB_DONE.into(),
            received: 10,
            expected: None,
            retry: 0,
            reason: None,
            created_by: None,
            created_at: 0,
            updated_at: 0,
            sessions: Some(1),
            rejected_chunks: Some(0),
            future_dropped: Some(0),
            crc_failed: None,
            compared: Some(8),
            disagreed: Some(2),
        };
        let row = job_ctx(&job, None);
        assert_eq!(row.label, "ABCDEF");
        assert_eq!(
            row.samples_expected,
            Some(11),
            "estimated from the interval"
        );
        assert_eq!(row.samples_interval_min, Some(15));
        assert!(row.samples_interval_assumed);
        assert_eq!(row.status, "done", "a disagreement does not fail a job");
    }

    /// Hardware test 2026-10-09: two Beacons, similar windows, 7381 and 2460
    /// expected. Both were the window over the Beacon's own interval (5 and
    /// 15 min); the row now counts the same way before the Hub reports and
    /// says which interval it counted at.
    #[test]
    fn the_expected_count_is_the_window_over_the_beacons_interval() {
        let window = 7381 * 300 - 120;
        let job = |expected| BeaconHistoryJob {
            id: "bh-01J0000000000000000ABCDEF".into(),
            hostname: "hub".into(),
            mac: "AA:BB:CC:DD:EE:01".into(),
            from_ts: Some(1_757_800_000),
            to_ts: Some(1_757_800_000 + window),
            trigger: MANUAL_TRIGGER.into(),
            state: "receiving".into(),
            received: 0,
            expected,
            retry: 0,
            reason: None,
            created_by: None,
            created_at: 0,
            updated_at: 0,
            sessions: None,
            rejected_chunks: None,
            future_dropped: None,
            crc_failed: None,
            compared: None,
            disagreed: None,
        };
        // Before the Hub reports: counted like the Hub, both ends inclusive.
        let row = job_ctx(&job(None), Some(300));
        assert_eq!(row.samples_expected, Some(7381));
        assert_eq!(row.samples_interval_min, Some(5));
        assert!(!row.samples_interval_assumed);
        // The Hub's number stands and keeps its interval.
        let row = job_ctx(&job(Some(7381)), Some(300));
        assert_eq!(row.samples_expected, Some(7381));
        assert_eq!(row.samples_interval_min, Some(5));
        // An interval the Hub did not count with is not claimed for its number.
        let row = job_ctx(&job(Some(7381)), Some(900));
        assert_eq!(row.samples_expected, Some(7381));
        assert_eq!(row.samples_interval_min, None);
        // Unknown interval: the default, said to be assumed.
        let row = job_ctx(&job(None), None);
        assert_eq!(row.samples_expected, Some(2461));
        assert_eq!(row.samples_interval_min, Some(15));
        assert!(row.samples_interval_assumed);
    }
}
