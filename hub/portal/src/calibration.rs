//! Calibration certificates of Nodes, Beacons and the Hub's wired probes,
//! and the reminder that follows their dates (plan/portal/calibration.cs.md,
//! GitLab #99; probes since the amendment 2026-10-14, keyed by Hub + line).
//!
//! Three things live here, and the routes (`routes::calibration_routes`)
//! stay thin over them:
//!
//! 1. **The files.** One PDF per upload at
//!    `{data_dir}/calibrations/{scope}/{entity}/{id}.pdf` — inside the data
//!    directory the Landlock ruleset already grants read/write
//!    (`sandbox.rs`), so the feature adds no rule. Written to a temp name,
//!    synced and renamed, so a reader never sees half a certificate. The
//!    row (`device_calibrations`, V35) is inserted after the rename; V35's
//!    header says why that order.
//! 2. **The level.** A device's current certificate is its newest row not
//!    deleted, and its expiry date (`valid_until`, the first day it is no
//!    longer valid — amendment 2026-10-13) against today decides one of four
//!    [`CalibrationLevel`]s, from the YAML's
//!    `calibration.warning_days`/`second_warning_days`. The levels are
//!    cached in memory for the Node and Beacon lists, whose row renders are
//!    synchronous and per SSE swap — the same reason `OfflineService` keeps
//!    its verdicts in memory.
//! 3. **The alarm.** One `alarm_events` row per device (`alarm_type`
//!    `CALIBRATION`): warning inside the first warning window, still warning
//!    (a different sentence) inside the second, critical from the expiry
//!    date on; cleared by an upload, a date moved out of the warning window,
//!    or the deletion of the last certificate standing. Every move into a
//!    more urgent level — the onset, the second warning, the expiry — is
//!    e-mailed once through the notifier, the way the offline sweep's onsets
//!    are. Which level the open row is at is read back from the row itself
//!    ([`level_of_message`]), so a restart does not send a level's e-mail
//!    again.
//!
//! An admin may delete a certificate, with a required reason (amendment
//! 2026-10-13): the PDF goes, the row stays marked as deleted, and the
//! device's alarm is recomputed against the certificate that is current
//! after it ([`CalibrationService::delete`]).
//!
//! # Why the alarm is not a fault
//!
//! Decision 2026-09-25 (§3): a lapsing certificate is an operational task,
//! not a broken measurement. The alarm row therefore touches nothing the
//! device's value or connectivity state is derived from — `NodeState`,
//! `BeaconState` and the offline detector never see it — and the Faults
//! tiles (`render::page_stats_ctx`), which count live operational faults,
//! cannot count it. Its `sensor_id` is `cal:{scope}:{entity}` rather than
//! the `lrw:{key}:…`/`eye:{mac}:…` families on purpose: the probe window
//! and the History page read every row under those prefixes as a
//! measurement event of that probe, and a certificate is not one.
//!
//! # When the sweep runs
//!
//! Hourly, plus once for one device right after an upload or a date change
//! (§3). The hourly pass only writes alarms for devices the live snapshot
//! knows: right after a start the snapshot is empty until the retained MQTT
//! state lands, and "not in the snapshot" must not read as "removed", or
//! every restart would clear every open calibration alarm. A device really
//! removed through the portal has its alarm closed by the removal route
//! ([`CalibrationService::forget_device`]).

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, mpsc};

use portal_storage::{
    AlarmListFilter, AlarmSeverityChange, DeviceCalibration, NewAlarmEvent, NewDeviceCalibration,
    Storage, StorageError, THRESHOLD_SCOPE_EYE, THRESHOLD_SCOPE_NODE,
};

use crate::alarms::{AlarmNudge, NotifyJob};
use crate::config::CalibrationConfig;
use crate::db::Db;
use crate::notify::{SystemAlertEvent, SystemAlertKind};
use crate::state::{ChangeEvent, PortalState, StateStore};

/// Largest certificate accepted (§4: 10 MB). A lab certificate is a few
/// hundred kilobytes; this leaves room for a scanned one.
pub const MAX_PDF_BYTES: usize = 10 * 1024 * 1024;

/// Most certificates one device may hold (§4). Older ones are never removed
/// automatically, so this is what bounds the directory.
pub const MAX_PER_DEVICE: i64 = 50;

/// The optional note's cap, in characters.
pub const MAX_NOTE_CHARS: usize = 500;

/// The delete reason's cap, in characters; at least one is required.
pub const MAX_DELETE_MESSAGE_CHARS: usize = 500;

/// `alarm_events.alarm_type` of the reminder — `persist::alarm_type_str`'s
/// spelling of `portal_domain::AlarmType::Calibration`.
pub const ALARM_TYPE: &str = "CALIBRATION";

/// Every PDF starts with this header (ISO 32000-1 §7.5.2). Checked instead
/// of trusting the browser's `Content-Type`, which is whatever the client
/// says.
const PDF_MAGIC: &[u8] = b"%PDF-";

/// How often the reminder sweep wakes. The levels change once a day at UTC
/// midnight; an hour of lag on that is invisible next to a 7-day window.
const SWEEP_PERIOD: Duration = Duration::from_secs(3600);

/// How long after start the first alarm pass waits — long enough for the
/// retained MQTT state to fill the snapshot (see the module doc).
const STARTUP_GRACE: Duration = Duration::from_secs(120);

const DAY_S: i64 = 86_400;

/// `calibration.second_warning_days` for the second warning's label, "Vyprší do N dní" (Hynek
/// 2026-09-25: the label follows the YAML). Set once by `main` before the server renders
/// anything; templates read it through `calibration_second_warning_days()`. The default is the
/// config default, which is what tests render with.
static LABEL_SECOND_WARNING_DAYS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(7);

pub fn set_label_second_warning_days(days: u32) {
    LABEL_SECOND_WARNING_DAYS.store(days, std::sync::atomic::Ordering::Relaxed);
}

pub fn label_second_warning_days() -> u32 {
    LABEL_SECOND_WARNING_DAYS.load(std::sync::atomic::Ordering::Relaxed)
}

/// `device_calibrations.scope` of a Hub probe (V38). Not `hub`: that is the
/// Hub itself in `device_meta`.
pub const SCOPE_PROBE: &str = "probe";

/// Which device kind a certificate belongs to. `as_str` is the
/// `device_meta` scope the rows are keyed by, and `probe` for a Hub's wired
/// line (amendment 2026-10-14), whose entity is the wire line "0".."7".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CalibrationScope {
    Node,
    Beacon,
    Probe,
}

impl CalibrationScope {
    pub fn as_str(self) -> &'static str {
        match self {
            CalibrationScope::Node => THRESHOLD_SCOPE_NODE,
            CalibrationScope::Beacon => THRESHOLD_SCOPE_EYE,
            CalibrationScope::Probe => SCOPE_PROBE,
        }
    }

    fn from_row(scope: &str) -> Option<Self> {
        match scope {
            THRESHOLD_SCOPE_NODE => Some(CalibrationScope::Node),
            THRESHOLD_SCOPE_EYE => Some(CalibrationScope::Beacon),
            SCOPE_PROBE => Some(CalibrationScope::Probe),
            _ => None,
        }
    }

    /// How an e-mail names the device kind.
    fn noun(self) -> &'static str {
        match self {
            CalibrationScope::Node => "Node",
            CalibrationScope::Beacon => "Beacon",
            CalibrationScope::Probe => "Probe",
        }
    }
}

/// The directory one device's PDFs live in, or `None` if its identity could
/// escape it. A probe's line is not unique across Hubs, so its directory
/// has the (lowercase) hostname above the line; Nodes and Beacons are
/// unique on their own and keep V35's `{scope}/{entity}`.
fn device_dir(root: &Path, hostname: &str, scope: &str, entity: &str) -> Option<PathBuf> {
    let mut dir = root.join(safe_segment(scope)?);
    if scope == SCOPE_PROBE {
        dir = dir.join(safe_segment(&hostname.to_ascii_lowercase())?);
    }
    Some(dir.join(safe_segment(entity)?))
}

/// One device, as the rows and the alarm key it: the owning Hub's hostname,
/// the kind, and the lowercase `node_key` or uppercase MAC.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceKey {
    pub hostname: String,
    pub scope: CalibrationScope,
    pub entity: String,
}

impl DeviceKey {
    pub fn new(hostname: &str, scope: CalibrationScope, entity: &str) -> Self {
        Self {
            hostname: hostname.to_string(),
            scope,
            entity: entity.to_string(),
        }
    }

    fn of_row(row: &DeviceCalibration) -> Option<Self> {
        Some(Self::new(
            &row.hostname,
            CalibrationScope::from_row(&row.scope)?,
            &row.entity,
        ))
    }

    /// The `alarm_events.sensor_id` of this device's reminder. Its own
    /// prefix, not the device's measurement one — see the module doc.
    pub fn alarm_sensor_id(&self) -> String {
        format!("cal:{}:{}", self.scope.as_str(), self.entity)
    }

    /// How an e-mail names the device after its name: the DevEUI or MAC, and
    /// for a probe its Hub and the line as the UI counts it ("hub-1 #3"),
    /// since a bare wire line says nothing outside the Hub's page.
    fn mail_entity(&self) -> String {
        match (self.scope, self.entity.parse::<u8>()) {
            (CalibrationScope::Probe, Ok(line)) => format!("{} #{}", self.hostname, line + 1),
            _ => self.entity.clone(),
        }
    }

    /// The device a reminder row belongs to — [`Self::alarm_sensor_id`]
    /// read back. The entity is the rest after the scope: a MAC has colons.
    fn of_alarm(hostname: &str, sensor_id: &str) -> Option<Self> {
        let (scope, entity) = sensor_id.strip_prefix("cal:")?.split_once(':')?;
        Some(Self::new(
            hostname,
            CalibrationScope::from_row(scope)?,
            entity,
        ))
    }
}

/// The four states of a current certificate (§2 as amended 2026-10-13).
/// Declared from the calmest to the most urgent: the order is what "moved
/// into a more urgent level" — the e-mail trigger — compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CalibrationLevel {
    /// "Platná": more than `warning_days` before the expiry date.
    Valid,
    /// "Brzy vyprší", the first warning: `warning_days` or fewer left.
    Warning,
    /// "Vyprší do N dní" (N = `second_warning_days`), the second warning:
    /// `second_warning_days` or fewer left. Still a warning, not critical.
    SecondWarning,
    /// "Prošlá": the expiry date has come — the one critical state.
    Expired,
}

impl CalibrationLevel {
    /// The template's and the i18n keys' spelling
    /// (`monitoring.calibration.state_{..}`).
    pub fn as_str(self) -> &'static str {
        match self {
            CalibrationLevel::Valid => "valid",
            CalibrationLevel::Warning => "warning",
            CalibrationLevel::SecondWarning => "second_warning",
            CalibrationLevel::Expired => "expired",
        }
    }

    /// The alarm row's severity, `None` for a certificate that needs
    /// nothing.
    fn severity(self) -> Option<&'static str> {
        match self {
            CalibrationLevel::Valid => None,
            CalibrationLevel::Warning | CalibrationLevel::SecondWarning => Some("warning"),
            CalibrationLevel::Expired => Some("critical"),
        }
    }

    /// The e-mail entering this level sends, `None` for the level that
    /// needs nothing.
    fn mail_kind(self) -> Option<SystemAlertKind> {
        match self {
            CalibrationLevel::Valid => None,
            CalibrationLevel::Warning => Some(SystemAlertKind::CalibrationDue),
            CalibrationLevel::SecondWarning => Some(SystemAlertKind::CalibrationSecondWarning),
            CalibrationLevel::Expired => Some(SystemAlertKind::CalibrationExpired),
        }
    }
}

/// Whole UTC days from today to the expiry date: 1 on the last valid day,
/// 0 on the expiry date itself, negative after it.
pub fn days_left(valid_until: i64, now: i64) -> i64 {
    valid_until.div_euclid(DAY_S) - now.div_euclid(DAY_S)
}

/// The level of a certificate expiring on `valid_until` (unix seconds of
/// the expiry date's UTC midnight — the first day it is no longer valid),
/// as of `now`.
///
/// Both windows are inclusive: with the default 30/7 a certificate expiring
/// on 31 October warns from 1 October, warns the second time from 24
/// October, and is expired — critical — from 31 October itself.
pub fn level(valid_until: i64, now: i64, cfg: &CalibrationConfig) -> CalibrationLevel {
    let left = days_left(valid_until, now);
    if left <= 0 {
        CalibrationLevel::Expired
    } else if left <= i64::from(cfg.second_warning_days) {
        CalibrationLevel::SecondWarning
    } else if left <= i64::from(cfg.warning_days) {
        CalibrationLevel::Warning
    } else {
        CalibrationLevel::Valid
    }
}

/// A device's current certificate out of its rows (newest first, as
/// `history` returns them): the newest one not deleted.
pub fn current_of(rows: &[DeviceCalibration]) -> Option<&DeviceCalibration> {
    rows.iter().find(|r| !r.is_deleted())
}

/// `YYYY-MM-DD` (what `<input type="date">` submits) → unix seconds of that
/// day's UTC midnight. `None` for anything else, including a calendar date
/// that does not exist and a year no certificate is plausibly valid in.
pub fn parse_date(raw: &str) -> Option<i64> {
    let mut parts = raw.trim().splitn(3, '-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    if !(2000..=2199).contains(&year) {
        return None;
    }
    let date =
        time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()?;
    Some(date.midnight().assume_utc().unix_timestamp())
}

/// Unix seconds → `YYYY-MM-DD` (UTC). The alarm message and the date
/// input's value; the card itself renders through `short_dt("date")`.
pub fn format_date(unix: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp(unix) {
        Ok(t) => format!("{:04}-{:02}-{:02}", t.year(), u8::from(t.month()), t.day()),
        Err(_) => String::new(),
    }
}

/// Unix seconds → RFC 3339 (UTC), the shape `short_dt` reads.
pub fn rfc3339(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn iso_now() -> String {
    rfc3339(now_unix())
}

/// The sentence the alarm row carries (English, like every stored alarm
/// message — `offline::offline_message`; `i18n::translate_alarm_message`
/// draws each shape in the page's language). Worded against the date and
/// the configured window rather than a countdown, so an hourly pass does
/// not rewrite it every day.
fn alarm_message(level: CalibrationLevel, valid_until: i64, cfg: &CalibrationConfig) -> String {
    let date = format_date(valid_until);
    match level {
        CalibrationLevel::Expired => format!("{EXPIRED_PREFIX}{date}"),
        CalibrationLevel::SecondWarning => {
            let n = cfg.second_warning_days;
            let unit = if n == 1 { "day" } else { "days" };
            format!("{SECOND_WARNING_PREFIX}{n} {unit}, on {date}")
        }
        CalibrationLevel::Warning | CalibrationLevel::Valid => format!("{WARNING_PREFIX}{date}"),
    }
}

/// The three sentence openings [`alarm_message`] writes and
/// [`level_of_message`] reads back. `pub(crate)` for the translator.
pub(crate) const WARNING_PREFIX: &str = "Calibration expires on ";
pub(crate) const SECOND_WARNING_PREFIX: &str = "Calibration expires within ";
pub(crate) const EXPIRED_PREFIX: &str = "Calibration expired on ";

/// Which level an open reminder row is at, read from its sentence. The
/// severity alone cannot say it — both warnings are `warning` — and the
/// level is what decides whether a pass owes an e-mail, so it has to come
/// from the table, which survives a restart, and not from memory.
fn level_of_message(message: &str) -> Option<CalibrationLevel> {
    if message.starts_with(EXPIRED_PREFIX) {
        Some(CalibrationLevel::Expired)
    } else if message.starts_with(SECOND_WARNING_PREFIX) {
        Some(CalibrationLevel::SecondWarning)
    } else if message.starts_with(WARNING_PREFIX) {
        Some(CalibrationLevel::Warning)
    } else {
        None
    }
}

// ---- files ---------------------------------------------------------------

/// A path segment built from a device identity, or `None` if it could
/// escape its directory. The routes only ever pass a `node_key` or a MAC the
/// store knows, so this is the second line, not the first.
fn safe_segment(s: &str) -> Option<&str> {
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_'));
    ok.then_some(s)
}

/// An upload id is 32 lowercase hex digits (`Uuid::simple`), and nothing
/// else reaches the file system as a name.
fn is_upload_id(id: &str) -> bool {
    id.len() == 32 && id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// The client's file name as something safe to show and to put in a
/// `Content-Disposition`: the last path component, no control characters or
/// quotes, capped, with a fallback when nothing is left.
fn display_file_name(raw: Option<&str>) -> String {
    let base = raw
        .unwrap_or("")
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control() && *c != '"')
        .take(200)
        .collect::<String>();
    let base = base.trim();
    if base.is_empty() {
        "certificate.pdf".to_string()
    } else {
        base.to_string()
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String is infallible.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Writes `bytes` as `{dir}/{id}.pdf` without ever exposing a partial file:
/// temp name, `fsync`, rename, then the directory's `fsync` so the rename
/// itself survives a power cut.
fn write_atomically(dir: &Path, id: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{id}.pdf.tmp"));
    let dest = dir.join(format!("{id}.pdf"));
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &dest)
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(dest)
}

// ---- the alarm row -----------------------------------------------------

/// What one device's reminder row should say, resolved before the write so
/// the write itself is a pure function of the table ([`apply_alarm`]).
#[derive(Debug, Clone)]
struct AlarmPlan {
    hostname: String,
    sensor_id: String,
    /// `None` = no alarm should be open.
    desired: Option<DesiredAlarm>,
    display_name: String,
    display_location: Option<String>,
    at: String,
}

#[derive(Debug, Clone)]
struct DesiredAlarm {
    level: CalibrationLevel,
    severity: &'static str,
    message: String,
}

/// What [`apply_alarm`] did to a device's reminder row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlarmOutcome {
    /// Nothing open and nothing needed, or the open row already says it.
    Unchanged,
    /// A row was opened, at this level.
    Opened { id: String, level: CalibrationLevel },
    /// The open row moved into a more urgent level: the second warning, or
    /// the expiry.
    Escalated { id: String, level: CalibrationLevel },
    /// The open row changed otherwise: back to a calmer level after a date
    /// edit, or a new date inside the same level.
    Updated { id: String },
    /// The open row was cleared.
    Cleared { id: String },
}

/// Opens, updates or clears one device's reminder so the table matches
/// `plan`. Idempotent: the decision is taken against the row open right
/// now, so running it twice writes once.
fn apply_alarm(storage: &Storage, plan: &AlarmPlan) -> Result<AlarmOutcome, StorageError> {
    let open = storage.active_alarm(&plan.hostname, &plan.sensor_id)?;
    Ok(match (&plan.desired, open) {
        (None, None) => AlarmOutcome::Unchanged,
        (None, Some(open)) => {
            storage.clear_alarm(&open.id, &plan.at, None)?;
            AlarmOutcome::Cleared { id: open.id }
        }
        (Some(want), None) => {
            let id = storage.insert_alarm_event(&NewAlarmEvent {
                hostname: plan.hostname.clone(),
                sensor_id: plan.sensor_id.clone(),
                sensor_name: Some(plan.display_name.clone()),
                sensor_location: plan.display_location.clone(),
                alarm_type: ALARM_TYPE.to_string(),
                severity: want.severity.to_string(),
                from_state: Some("NORMAL".to_string()),
                to_state: Some(want.severity.to_ascii_uppercase()),
                message: Some(want.message.clone()),
                // A date, not a measurement: nothing to put here.
                value: None,
                threshold: None,
                triggered_at: plan.at.clone(),
                cleared_at: None,
                acknowledged_at: None,
                acknowledged_by_user_id: None,
                acknowledged_by_username: None,
                native_key: None,
            })?;
            AlarmOutcome::Opened {
                id,
                level: want.level,
            }
        }
        (Some(want), Some(open)) => {
            if open.severity == want.severity && open.message.as_deref() == Some(&want.message) {
                AlarmOutcome::Unchanged
            } else {
                // A sentence this module did not write (none should exist)
                // reads as the calmest level its severity allows, so an
                // unknown row errs towards one more e-mail, not one fewer.
                let was = open
                    .message
                    .as_deref()
                    .and_then(level_of_message)
                    .unwrap_or(if open.severity == "critical" {
                        CalibrationLevel::Expired
                    } else {
                        CalibrationLevel::Warning
                    });
                let to_state = want.severity.to_ascii_uppercase();
                storage.update_alarm_severity(
                    &open.id,
                    AlarmSeverityChange {
                        severity: want.severity,
                        alarm_type: ALARM_TYPE,
                        to_state: Some(&to_state),
                        value: None,
                        threshold: None,
                        message: Some(&want.message),
                        // An escalation to critical is one nobody acknowledged (decisions.md #28c).
                        clear_acknowledgement: want.severity == "critical"
                            && open.severity != "critical",
                    },
                )?;
                if want.level > was {
                    AlarmOutcome::Escalated {
                        id: open.id,
                        level: want.level,
                    }
                } else {
                    AlarmOutcome::Updated { id: open.id }
                }
            }
        }
    })
}

/// The name and location the rest of the portal shows for this device, or
/// `None` when the live snapshot does not hold it (or holds only an
/// unregistered Beacon candidate) — which is also the sweep's test for
/// "may I write this device's alarm".
fn device_display(snapshot: &PortalState, key: &DeviceKey) -> Option<(String, Option<String>)> {
    let device = snapshot.device(&key.hostname)?;
    match key.scope {
        CalibrationScope::Node => {
            let n = device.nodes.get(&key.entity)?;
            Some((
                n.name.clone().unwrap_or_else(|| key.entity.clone()),
                n.location.clone(),
            ))
        }
        CalibrationScope::Beacon => {
            let b = device
                .ble_sensors
                .get(&key.entity)
                .filter(|b| b.discovered != Some(true))?;
            Some((
                b.name.clone().unwrap_or_else(|| key.entity.clone()),
                b.location.clone(),
            ))
        }
        CalibrationScope::Probe => {
            let line: usize = key.entity.parse().ok()?;
            let s = device.sensors.get(line)?;
            Some((
                s.name
                    .clone()
                    .unwrap_or_else(|| format!("Line {}", line + 1)),
                s.location.clone(),
            ))
        }
    }
}

// ---- the service -------------------------------------------------------

/// Why an upload was refused. Each maps to the card's flash message.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("no file in the upload")]
    NoFile,
    #[error("the file is not a PDF")]
    NotPdf,
    #[error("the file is larger than {MAX_PDF_BYTES} bytes")]
    TooLarge,
    #[error("the device already holds {MAX_PER_DEVICE} certificates")]
    LimitReached,
    #[error("the note is longer than {MAX_NOTE_CHARS} characters")]
    NoteTooLong,
    #[error("the device identity cannot name a directory")]
    BadIdentity,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl UploadError {
    /// The `monitoring.calibration.err_*` key the card shows.
    pub fn message_key(&self) -> &'static str {
        match self {
            UploadError::NoFile => "monitoring.calibration.err_no_file",
            UploadError::NotPdf => "monitoring.calibration.err_not_pdf",
            UploadError::TooLarge => "monitoring.calibration.err_too_large",
            UploadError::LimitReached => "monitoring.calibration.err_limit",
            UploadError::NoteTooLong => "monitoring.calibration.err_note_too_long",
            UploadError::BadIdentity | UploadError::Io(_) | UploadError::Storage(_) => {
                "monitoring.calibration.err_save_failed"
            }
        }
    }
}

/// Why a delete was refused. Each maps to the card's flash message.
#[derive(Debug, thiserror::Error)]
pub enum DeleteError {
    #[error("the delete reason is required")]
    MessageRequired,
    #[error("the delete reason is longer than {MAX_DELETE_MESSAGE_CHARS} characters")]
    MessageTooLong,
    /// Unknown id, another device's, or deleted already.
    #[error("no such certificate on this device")]
    NotFound,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl DeleteError {
    /// The `monitoring.calibration.err_*` key the card shows.
    pub fn message_key(&self) -> &'static str {
        match self {
            DeleteError::MessageRequired => "monitoring.calibration.err_delete_message",
            DeleteError::MessageTooLong => "monitoring.calibration.err_delete_message_too_long",
            DeleteError::NotFound => "monitoring.calibration.err_delete_not_found",
            DeleteError::Io(_) | DeleteError::Storage(_) => {
                "monitoring.calibration.err_delete_failed"
            }
        }
    }
}

/// A delete that went through: the row as it now reads (marked deleted)
/// and whether it was the device's current certificate.
#[derive(Debug, Clone)]
pub struct Deleted {
    pub row: DeviceCalibration,
    pub was_current: bool,
}

/// The delete form's reason, trimmed and checked: 1 to
/// [`MAX_DELETE_MESSAGE_CHARS`] characters.
pub fn delete_message(raw: &str) -> Result<&str, DeleteError> {
    let message = raw.trim();
    if message.is_empty() {
        Err(DeleteError::MessageRequired)
    } else if message.chars().count() > MAX_DELETE_MESSAGE_CHARS {
        Err(DeleteError::MessageTooLong)
    } else {
        Ok(message)
    }
}

/// One validated-by-the-route upload: the multipart fields, already read.
pub struct Upload {
    pub file_name: Option<String>,
    pub bytes: Vec<u8>,
    pub valid_until: i64,
    pub note: Option<String>,
}

/// The files, the level cache and the reminder. Cheap to clone (`Arc`s and
/// channel senders), like every other service on `AppState`.
#[derive(Clone)]
pub struct CalibrationService {
    /// The request path's connection: the sweep is one bulk read an hour
    /// plus a handful of alarm writes, the same argument `OfflineService`
    /// makes for sharing it.
    db: Db,
    config: CalibrationConfig,
    /// `{data_dir}/calibrations`.
    root: PathBuf,
    store: Arc<StateStore>,
    /// Current level per device with a certificate. `parking_lot`, held for
    /// a map lookup or swap only, never across an await.
    levels: Arc<RwLock<HashMap<DeviceKey, CalibrationLevel>>>,
    /// The current certificate's expiry date (unix, UTC midnight) per
    /// device, kept beside `levels`: the probe rows' "do {datum}" (UI
    /// amendment 2026-10-14).
    expiries: Arc<RwLock<HashMap<DeviceKey, i64>>>,
    /// Hubs (lowercase hostname) removed or unpaired through the portal
    /// since start (amendment 2026-10-14): their probes' reminders are
    /// closed and the sweep does not reopen them while the Hub lingers in
    /// the snapshot. A new probe upload on that Hub lifts it.
    forgotten_hubs: Arc<RwLock<std::collections::HashSet<String>>>,
    /// Wired after construction ([`Self::wire`]); `None` in a bare test
    /// instance, where delivery and SSE nudges are skipped.
    notify_tx: Option<mpsc::UnboundedSender<NotifyJob>>,
    nudge_tx: Option<broadcast::Sender<AlarmNudge>>,
}

impl CalibrationService {
    pub fn new(db: Db, config: CalibrationConfig, data_dir: &str, store: Arc<StateStore>) -> Self {
        Self {
            db,
            config,
            root: Path::new(data_dir).join("calibrations"),
            store,
            levels: Arc::new(RwLock::new(HashMap::new())),
            expiries: Arc::new(RwLock::new(HashMap::new())),
            forgotten_hubs: Arc::new(RwLock::new(std::collections::HashSet::new())),
            notify_tx: None,
            nudge_tx: None,
        }
    }

    /// Attaches the notifier (the onset and escalation e-mails) and the
    /// alarm table's SSE nudge.
    pub fn wire(
        mut self,
        notify_tx: mpsc::UnboundedSender<NotifyJob>,
        nudge_tx: broadcast::Sender<AlarmNudge>,
    ) -> Self {
        self.notify_tx = Some(notify_tx);
        self.nudge_tx = Some(nudge_tx);
        self
    }

    pub fn config(&self) -> CalibrationConfig {
        self.config
    }

    /// The cached level of every device of one kind on one Hub that holds a
    /// certificate, keyed by entity — what a list pass hands its rows.
    pub fn levels_for(
        &self,
        hostname: &str,
        scope: CalibrationScope,
    ) -> HashMap<String, CalibrationLevel> {
        self.levels
            .read()
            .iter()
            .filter(|(k, _)| k.scope == scope && k.hostname.eq_ignore_ascii_case(hostname))
            .map(|(k, level)| (k.entity.clone(), *level))
            .collect()
    }

    /// The cached expiry date of every probe of one Hub holding a
    /// certificate, keyed by line.
    pub fn probe_expiries(&self, hostname: &str) -> HashMap<String, i64> {
        self.expiries
            .read()
            .iter()
            .filter(|(k, _)| {
                k.scope == CalibrationScope::Probe && k.hostname.eq_ignore_ascii_case(hostname)
            })
            .map(|(k, until)| (k.entity.clone(), *until))
            .collect()
    }

    /// One device's certificates, newest first, deleted ones included; the
    /// current one is the first not deleted ([`current_of`]).
    pub async fn history(&self, key: &DeviceKey) -> Result<Vec<DeviceCalibration>, StorageError> {
        let key = key.clone();
        self.db
            .run(move |s| {
                s.list_device_calibrations(&key.hostname, key.scope.as_str(), &key.entity)
            })
            .await
    }

    /// Stores one certificate and makes it the device's current one, then
    /// re-checks that device's reminder (§3: "hned po nahrání").
    pub async fn upload(
        &self,
        key: &DeviceKey,
        uploaded_by: &str,
        upload: Upload,
    ) -> Result<DeviceCalibration, UploadError> {
        if upload.bytes.is_empty() {
            return Err(UploadError::NoFile);
        }
        if upload.bytes.len() > MAX_PDF_BYTES {
            return Err(UploadError::TooLarge);
        }
        if !upload.bytes.starts_with(PDF_MAGIC) {
            return Err(UploadError::NotPdf);
        }
        if upload
            .note
            .as_deref()
            .is_some_and(|n| n.chars().count() > MAX_NOTE_CHARS)
        {
            return Err(UploadError::NoteTooLong);
        }
        let Some(dir) = device_dir(&self.root, &key.hostname, key.scope.as_str(), &key.entity)
        else {
            return Err(UploadError::BadIdentity);
        };

        let held = ({
            let key = key.clone();
            self.db.run(move |s| {
                s.count_device_calibrations(&key.hostname, key.scope.as_str(), &key.entity)
            })
        })
        .await?;
        if held >= MAX_PER_DEVICE {
            return Err(UploadError::LimitReached);
        }

        let id = uuid::Uuid::new_v4().simple().to_string();
        let sha256 = hex(&Sha256::digest(&upload.bytes));
        let row = NewDeviceCalibration {
            id: id.clone(),
            hostname: key.hostname.clone(),
            scope: key.scope.as_str().to_string(),
            entity: key.entity.clone(),
            valid_until: upload.valid_until,
            file_name: display_file_name(upload.file_name.as_deref()),
            file_size: upload.bytes.len() as i64,
            sha256,
            note: upload.note,
            uploaded_by: uploaded_by.to_string(),
        };

        let path = {
            let (id, bytes) = (id.clone(), upload.bytes);
            tokio::task::spawn_blocking(move || write_atomically(&dir, &id, &bytes))
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))??
        };

        let inserted = self
            .db
            .write(move |s| s.insert_device_calibration(&row))
            .await;
        if let Err(err) = inserted {
            // No row names the file, so nothing would ever serve or count
            // it: take it back rather than leave an orphan behind.
            let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(path)).await;
            return Err(err.into());
        }

        let stored = {
            let id = id.clone();
            self.db.run(move |s| s.get_device_calibration(&id)).await?
        };
        if key.scope == CalibrationScope::Probe {
            // Someone works with this Hub's probes again: it is back.
            self.forgotten_hubs
                .write()
                .remove(&key.hostname.to_ascii_lowercase());
        }
        self.check_device(key, now_unix()).await;
        stored
            .ok_or_else(|| std::io::Error::other("the calibration row just written is gone").into())
    }

    /// §2's "Upravit datum": moves the current certificate's date and
    /// re-checks the reminder. `Ok(None)` when the device has no
    /// certificate to move; otherwise `(id, previous valid_until)`.
    pub async fn set_valid_until(
        &self,
        key: &DeviceKey,
        valid_until: i64,
    ) -> Result<Option<(String, i64)>, StorageError> {
        let changed = ({
            let key = key.clone();
            self.db.write(move |s| {
                s.update_current_calibration_valid_until(
                    &key.hostname,
                    key.scope.as_str(),
                    &key.entity,
                    valid_until,
                )
            })
        })
        .await?;
        if changed.is_some() {
            self.check_device(key, now_unix()).await;
        }
        Ok(changed)
    }

    /// This device's row with this id, or `None` for an id that is
    /// malformed, unknown or another device's.
    async fn owned_row(&self, key: &DeviceKey, id: &str) -> Option<DeviceCalibration> {
        if !is_upload_id(id) {
            return None;
        }
        let row = ({
            let id = id.to_string();
            self.db.run(move |s| s.get_device_calibration(&id))
        })
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, id, "calibration row unreadable");
            None
        })?;
        let owned = row.hostname.eq_ignore_ascii_case(&key.hostname)
            && row.scope == key.scope.as_str()
            && row.entity == key.entity;
        owned.then_some(row)
    }

    /// Where a row's PDF lives, or `None` if its identity cannot name a
    /// directory.
    fn file_path(&self, row: &DeviceCalibration) -> Option<PathBuf> {
        Some(
            device_dir(&self.root, &row.hostname, &row.scope, &row.entity)?
                .join(format!("{}.pdf", row.id)),
        )
    }

    /// One stored certificate of this device and its bytes, or `None` for
    /// an id that is malformed, unknown, another device's, deleted, or whose
    /// file is gone.
    pub async fn certificate(
        &self,
        key: &DeviceKey,
        id: &str,
    ) -> Option<(DeviceCalibration, Vec<u8>)> {
        let row = self.owned_row(key, id).await?;
        if row.is_deleted() {
            return None;
        }
        let path = self.file_path(&row)?;
        match tokio::task::spawn_blocking(move || std::fs::read(path)).await {
            Ok(Ok(bytes)) => Some((row, bytes)),
            Ok(Err(err)) => {
                tracing::warn!(%err, id = row.id, "calibration row without its file");
                None
            }
            Err(err) => {
                tracing::warn!(%err, "calibration read task failed");
                None
            }
        }
    }

    /// An admin's delete (amendment 2026-10-13): the PDF goes, the row is
    /// marked with who, when and `message`, and the device's reminder is
    /// recomputed at once against the certificate current after it — the
    /// previous one standing, or none, which closes the alarm.
    ///
    /// The file goes first and a missing file is not an error, so a delete
    /// whose row write failed is finished by simply deleting again; the
    /// other order would leave a row marked deleted over a PDF still on
    /// disk, with nothing that ever removes it.
    pub async fn delete(
        &self,
        key: &DeviceKey,
        id: &str,
        deleted_by: &str,
        message: &str,
    ) -> Result<Deleted, DeleteError> {
        let message = delete_message(message)?.to_string();
        let row = self.owned_row(key, id).await.ok_or(DeleteError::NotFound)?;
        if row.is_deleted() {
            return Err(DeleteError::NotFound);
        }
        let was_current =
            current_of(&self.history(key).await?).map(|c| c.id.as_str()) == Some(row.id.as_str());

        let path = self
            .file_path(&row)
            .ok_or_else(|| std::io::Error::other("the calibration row names no directory"))?;
        tokio::task::spawn_blocking(move || match std::fs::remove_file(&path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
            _ => {
                if let Some(dir) = path.parent().and_then(|d| std::fs::File::open(d).ok()) {
                    let _ = dir.sync_all();
                }
                Ok(())
            }
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))??;

        let marked = ({
            let (id, by) = (row.id.clone(), deleted_by.to_string());
            self.db
                .write(move |s| s.mark_device_calibration_deleted(&id, &by, &message))
        })
        .await?;
        if !marked {
            // Another delete of the same row won the race.
            return Err(DeleteError::NotFound);
        }
        self.check_device(key, now_unix()).await;
        let row = ({
            let id = row.id.clone();
            self.db.run(move |s| s.get_device_calibration(&id))
        })
        .await?
        .ok_or(DeleteError::NotFound)?;
        Ok(Deleted { row, was_current })
    }

    /// The reminder pass over every device with a certificate: refresh the
    /// level cache, bring each known device's alarm in line, and close any
    /// reminder left open on a device with no certificate standing.
    pub async fn check_all(&self, now: i64) -> Vec<(DeviceKey, AlarmOutcome)> {
        let rows = match self.db.run(|s| s.list_current_calibrations()).await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(%err, "calibration sweep skipped: rows unreadable");
                return Vec::new();
            }
        };

        let current: Vec<(DeviceKey, DeviceCalibration, CalibrationLevel)> = rows
            .into_iter()
            .filter_map(|row| {
                let key = DeviceKey::of_row(&row)?;
                let level = level(row.valid_until, now, &self.config);
                Some((key, row, level))
            })
            .collect();

        let fresh: HashMap<DeviceKey, CalibrationLevel> = current
            .iter()
            .map(|(k, _, level)| (k.clone(), *level))
            .collect();
        *self.expiries.write() = current
            .iter()
            .map(|(k, row, _)| (k.clone(), row.valid_until))
            .collect();
        let previous = std::mem::replace(&mut *self.levels.write(), fresh.clone());
        for (key, level) in &fresh {
            if previous.get(key) != Some(level) {
                self.repaint(key);
            }
        }

        for key in previous.keys().filter(|k| !fresh.contains_key(*k)) {
            self.repaint(key);
        }

        let snapshot = self.store.snapshot();
        let mut outcomes = Vec::new();
        for (key, row, level) in current {
            let Some(display) = self.display(&snapshot, &key) else {
                continue;
            };
            let outcome = self.reconcile(&key, Some((&row, level)), display).await;
            outcomes.push((key, outcome));
        }

        // A reminder whose device has no certificate standing any more —
        // its last one was deleted and the delete's own recheck did not get
        // to write. Unlike the pass above this needs no snapshot: "no
        // certificate" is the table's fact, not the live state's.
        let open = self
            .db
            .run(|s| {
                s.list_alarms(&AlarmListFilter {
                    limit: 10_000,
                    include_cleared: false,
                    include_acknowledged: true,
                    alarm_type: Some(ALARM_TYPE.to_string()),
                    ..Default::default()
                })
            })
            .await;
        match open {
            Ok((open, _)) => {
                for alarm in open {
                    let Some(key) = DeviceKey::of_alarm(&alarm.hostname, &alarm.sensor_id) else {
                        continue;
                    };
                    if !fresh.contains_key(&key) {
                        // Re-read rather than close outright: an upload that
                        // landed since the bulk read above is current now,
                        // and closing its fresh alarm would mail its onset a
                        // second time on the next pass.
                        if let Some(outcome) = self.check_device(&key, now).await {
                            outcomes.push((key, outcome));
                        }
                    }
                }
            }
            Err(err) => tracing::warn!(%err, "calibration sweep: open reminders unreadable"),
        }
        outcomes
    }

    /// The same pass for one device, right after its certificate changed.
    /// A device left with no certificate standing (its last one deleted)
    /// has its reminder closed, whether the snapshot holds it or not.
    pub async fn check_device(&self, key: &DeviceKey, now: i64) -> Option<AlarmOutcome> {
        let rows = match self.history(key).await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(%err, entity = key.entity, "calibration check skipped");
                return None;
            }
        };
        let Some(current) = current_of(&rows) else {
            self.expiries.write().remove(key);
            if self.levels.write().remove(key).is_some() {
                self.repaint(key);
            }
            return Some(self.close_alarm(key).await);
        };
        let level = level(current.valid_until, now, &self.config);
        self.expiries
            .write()
            .insert(key.clone(), current.valid_until);
        let changed = self.levels.write().insert(key.clone(), level) != Some(level);
        if changed {
            self.repaint(key);
        }
        let display = self.display(&self.store.snapshot(), key)?;
        Some(self.reconcile(key, Some((current, level)), display).await)
    }

    /// The device was removed through the portal: close its reminder, which
    /// the hourly pass would otherwise leave open for good once the device
    /// leaves the snapshot. The certificates themselves stay (§4).
    /// [`device_display`], except that a probe of a Hub forgotten through
    /// [`Self::forget_hub`] has no alarm to write.
    fn display(&self, snapshot: &PortalState, key: &DeviceKey) -> Option<(String, Option<String>)> {
        if key.scope == CalibrationScope::Probe
            && self
                .forgotten_hubs
                .read()
                .contains(&key.hostname.to_ascii_lowercase())
        {
            return None;
        }
        device_display(snapshot, key)
    }

    /// A Hub was removed from the cluster or unpaired (amendment
    /// 2026-10-14): close every probe reminder it holds. The certificates
    /// stay (§4), and so does the levels cache, which only draws icons.
    pub async fn forget_hub(&self, hostname: &str) {
        self.forgotten_hubs
            .write()
            .insert(hostname.to_ascii_lowercase());
        for line in 0..8u8 {
            let key = DeviceKey::new(hostname, CalibrationScope::Probe, &line.to_string());
            self.close_alarm(&key).await;
        }
    }

    /// The most urgent level among this Hub's probes holding a certificate
    /// — the Hubs list's icon (UI amendment 2026-10-14: drawn always, green
    /// when every certificate is valid); `None` when no probe holds one.
    pub fn worst_probe_level(&self, hostname: &str) -> Option<CalibrationLevel> {
        self.levels_for(hostname, CalibrationScope::Probe)
            .into_values()
            .max()
    }

    pub async fn forget_device(&self, key: &DeviceKey) {
        self.levels.write().remove(key);
        self.expiries.write().remove(key);
        self.close_alarm(key).await;
    }

    /// Clears the device's reminder, if one is open. No snapshot needed:
    /// nothing about the device is shown, only the row closed.
    async fn close_alarm(&self, key: &DeviceKey) -> AlarmOutcome {
        let plan = AlarmPlan {
            hostname: key.hostname.clone(),
            sensor_id: key.alarm_sensor_id(),
            desired: None,
            display_name: key.entity.clone(),
            display_location: None,
            at: iso_now(),
        };
        match self.db.write(move |s| apply_alarm(s, &plan)).await {
            Ok(outcome) => {
                if outcome != AlarmOutcome::Unchanged {
                    self.nudge(&key.hostname);
                }
                outcome
            }
            Err(err) => {
                tracing::warn!(%err, entity = key.entity, "calibration alarm left open; the next pass retries");
                AlarmOutcome::Unchanged
            }
        }
    }

    /// Spawns the hourly sweep. Call once from `main`.
    pub fn spawn_task(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            // Levels right away, so the list icons are there from the first
            // render; the alarm half finds an empty snapshot at this point
            // and writes nothing, which is the point of the grace below.
            this.check_all(now_unix()).await;
            let mut ticker =
                tokio::time::interval_at(tokio::time::Instant::now() + STARTUP_GRACE, SWEEP_PERIOD);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                this.check_all(now_unix()).await;
            }
        });
    }

    /// Writes one device's reminder for its current certificate and fans
    /// the change out: the alarm table's nudge, and one e-mail for every
    /// move into a more urgent level — the onset, the second warning, the
    /// expiry (§3 as amended 2026-10-13).
    async fn reconcile(
        &self,
        key: &DeviceKey,
        current: Option<(&DeviceCalibration, CalibrationLevel)>,
        (display_name, display_location): (String, Option<String>),
    ) -> AlarmOutcome {
        let desired = current.and_then(|(row, level)| {
            level.severity().map(|severity| DesiredAlarm {
                level,
                severity,
                message: alarm_message(level, row.valid_until, &self.config),
            })
        });
        let plan = AlarmPlan {
            hostname: key.hostname.clone(),
            sensor_id: key.alarm_sensor_id(),
            desired: desired.clone(),
            display_name: display_name.clone(),
            display_location,
            at: iso_now(),
        };
        let outcome = match self.db.write(move |s| apply_alarm(s, &plan)).await {
            Ok(outcome) => outcome,
            Err(err) => {
                // Housekeeping, not an edge: the next pass re-derives the
                // same decision from the table, so retry-only is enough.
                tracing::warn!(%err, entity = key.entity, "calibration alarm write failed; the next pass retries");
                return AlarmOutcome::Unchanged;
            }
        };

        if outcome != AlarmOutcome::Unchanged {
            self.nudge(&key.hostname);
        }
        let entered = match &outcome {
            AlarmOutcome::Opened { level, .. } | AlarmOutcome::Escalated { level, .. } => {
                Some(*level)
            }
            _ => None,
        };
        let mail = entered.and_then(|level| Some((level, level.mail_kind()?)));
        if let (Some((level, kind)), Some(want), Some((row, _)), Some(tx)) =
            (mail, &desired, current, &self.notify_tx)
        {
            // The e-mail is written from the catalogue in the installation's language;
            // `message` is the English sentence the delivery history keeps.
            let days = (level == CalibrationLevel::SecondWarning)
                .then_some(self.config.second_warning_days);
            // Czech says "do 1 dne" but "do 7 dní": the one-day text is its own entry.
            let msg_key = match days {
                Some(1) => "calibration_second_warning_one",
                _ => kind.as_str(),
            };
            let entity = key.mail_entity();
            let mut params = vec![
                ("device", key.scope.noun().to_string()),
                ("name", display_name.clone()),
                ("entity", entity.clone()),
                ("date", format_date(row.valid_until)),
            ];
            if let Some(days) = days {
                params.push(("days", days.to_string()));
            }
            let _ = tx.send(NotifyJob::SystemAlerts {
                hostname: key.hostname.clone(),
                events: vec![SystemAlertEvent::new(
                    kind,
                    format!(
                        "{} {display_name} ({entity}): {}",
                        key.scope.noun(),
                        want.message
                    ),
                    msg_key,
                    params,
                )],
            });
        }
        outcome
    }

    /// Repaints the alarm table and the sidebar badge. No toast
    /// (`activated_ids` empty): the e-mail and the device's own card carry
    /// the news, and a pop-up a month before a date would read as an
    /// emergency — the stance the liveness writer takes for its rows.
    fn nudge(&self, hostname: &str) {
        if let Some(tx) = &self.nudge_tx {
            let _ = tx.send(AlarmNudge {
                hostname: hostname.to_string(),
                activated_ids: Vec::new(),
                escalated_ids: Vec::new(),
            });
        }
    }

    /// Repaints the device's list row, whose calibration icon just changed.
    fn repaint(&self, key: &DeviceKey) {
        self.store.emit(match key.scope {
            CalibrationScope::Node => ChangeEvent::Node {
                hostname: key.hostname.clone(),
                node_key: key.entity.clone(),
            },
            CalibrationScope::Beacon => ChangeEvent::Beacon {
                hostname: key.hostname.clone(),
                mac: key.entity.clone(),
            },
            CalibrationScope::Probe => match key.entity.parse() {
                Ok(line) => ChangeEvent::SensorLine {
                    hostname: key.hostname.clone(),
                    line,
                },
                Err(_) => return,
            },
        });
    }
}

#[cfg(test)]
impl CalibrationService {
    /// A service with no notifier and no nudge behind it.
    pub fn test_instance(db: Db, data_dir: &str, store: Arc<StateStore>) -> Self {
        Self::new(db, CalibrationConfig::default(), data_dir, store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::events::{IngestEvent, NodeReadingMsg};

    const HOST: &str = "h1";
    const NODE: &str = "70b3d5e050011234";
    /// 2026-10-31T00:00:00Z.
    const OCT_31: i64 = 1_793_404_800;

    // ---- the level, the dates, the names -------------------------------

    /// §3 as amended 2026-10-13, inclusive at both ends: with 30/7 a
    /// certificate expiring on 31 October warns from 1 October (exactly
    /// `warning_days` before), warns the second time from 24 October
    /// (exactly `second_warning_days` before), is still a second warning on
    /// the 30th — the last valid day — and is expired on the 31st itself.
    #[test]
    fn the_level_follows_the_two_windows_day_by_day() {
        let cfg = CalibrationConfig::default();
        for (days_before, want) in [
            (31, CalibrationLevel::Valid),
            (30, CalibrationLevel::Warning),
            (8, CalibrationLevel::Warning),
            (7, CalibrationLevel::SecondWarning),
            (1, CalibrationLevel::SecondWarning),
            (0, CalibrationLevel::Expired),
            (-1, CalibrationLevel::Expired),
            (-400, CalibrationLevel::Expired),
        ] {
            // Both ends of the day, to prove the time of day does not move it.
            for at in [0, 23 * 3600 + 3599] {
                let now = OCT_31 - days_before * DAY_S + at;
                assert_eq!(level(OCT_31, now, &cfg), want, "{days_before} days before");
            }
        }

        let wide = CalibrationConfig {
            warning_days: 60,
            second_warning_days: 14,
        };
        assert_eq!(
            level(OCT_31, OCT_31 - 45 * DAY_S, &wide),
            CalibrationLevel::Warning
        );
        assert_eq!(
            level(OCT_31, OCT_31 - 14 * DAY_S, &wide),
            CalibrationLevel::SecondWarning
        );
        assert_eq!(
            level(OCT_31, OCT_31 - 15 * DAY_S, &wide),
            CalibrationLevel::Warning
        );
    }

    /// Only the expiry is critical; both warnings are warnings, and each
    /// level that needs attention has its own e-mail.
    #[test]
    fn only_the_expiry_is_critical_and_each_level_has_its_mail() {
        assert_eq!(CalibrationLevel::Valid.severity(), None);
        assert_eq!(CalibrationLevel::Warning.severity(), Some("warning"));
        assert_eq!(CalibrationLevel::SecondWarning.severity(), Some("warning"));
        assert_eq!(CalibrationLevel::Expired.severity(), Some("critical"));
        assert_eq!(CalibrationLevel::Valid.mail_kind(), None);
        assert_eq!(
            CalibrationLevel::Warning.mail_kind(),
            Some(SystemAlertKind::CalibrationDue)
        );
        assert_eq!(
            CalibrationLevel::SecondWarning.mail_kind(),
            Some(SystemAlertKind::CalibrationSecondWarning)
        );
        assert_eq!(
            CalibrationLevel::Expired.mail_kind(),
            Some(SystemAlertKind::CalibrationExpired)
        );
    }

    /// The row's sentence is how a pass knows which level is open, so every
    /// level's sentence must read back as that level.
    #[test]
    fn every_alarm_sentence_reads_back_as_its_level() {
        let cfg = CalibrationConfig::default();
        for level in [
            CalibrationLevel::Warning,
            CalibrationLevel::SecondWarning,
            CalibrationLevel::Expired,
        ] {
            let message = alarm_message(level, OCT_31, &cfg);
            assert_eq!(level_of_message(&message), Some(level), "{message}");
            assert!(message.contains("2026-10-31"), "{message}");
        }
        assert_eq!(
            alarm_message(CalibrationLevel::SecondWarning, OCT_31, &cfg),
            "Calibration expires within 7 days, on 2026-10-31"
        );
        let one = CalibrationConfig {
            warning_days: 30,
            second_warning_days: 1,
        };
        assert_eq!(
            alarm_message(CalibrationLevel::SecondWarning, OCT_31, &one),
            "Calibration expires within 1 day, on 2026-10-31"
        );
        assert_eq!(level_of_message("offline"), None);
        assert_eq!(
            DeviceKey::of_alarm(HOST, "cal:eye:AA:BB:CC:DD:EE:01"),
            Some(DeviceKey::new(
                HOST,
                CalibrationScope::Beacon,
                "AA:BB:CC:DD:EE:01"
            ))
        );
        assert_eq!(DeviceKey::of_alarm(HOST, "lrw:aa:temperature"), None);
    }

    #[test]
    fn dates_are_calendar_days_at_utc_midnight() {
        assert_eq!(parse_date("2026-10-31"), Some(OCT_31));
        assert_eq!(parse_date(" 2026-10-31 "), Some(OCT_31));
        assert_eq!(format_date(OCT_31), "2026-10-31");
        assert_eq!(
            format_date(parse_date("2028-02-29").expect("leap day")),
            "2028-02-29"
        );
        for bad in [
            "",
            "2026-02-30",
            "2026-13-01",
            "31. 10. 2026",
            "2026/10/31",
            "1999-12-31",
            "not a date",
        ] {
            assert_eq!(parse_date(bad), None, "{bad:?} must be refused");
        }
    }

    /// Amendment 2026-10-14: a probe is its Hub and wire line. The alarm key
    /// reads back, the directory has the Hub above the line (the line alone
    /// repeats on every Hub), and the e-mail names the line as the UI counts.
    #[test]
    fn a_probe_key_is_its_hub_and_line() {
        let key = DeviceKey::new("PROXIMOS-4E970F4A", CalibrationScope::Probe, "2");
        assert_eq!(key.alarm_sensor_id(), "cal:probe:2");
        assert_eq!(
            DeviceKey::of_alarm("PROXIMOS-4E970F4A", "cal:probe:2"),
            Some(key.clone())
        );
        assert_eq!(key.mail_entity(), "PROXIMOS-4E970F4A #3");
        assert_eq!(
            CalibrationScope::from_row("probe"),
            Some(CalibrationScope::Probe)
        );
        assert_eq!(CalibrationScope::from_row("hub"), None);

        let root = Path::new("/data/portal/calibrations");
        assert_eq!(
            device_dir(root, "PROXIMOS-4E970F4A", "probe", "2"),
            Some(root.join("probe").join("proximos-4e970f4a").join("2"))
        );
        assert_eq!(
            device_dir(root, "h1", "node", NODE),
            Some(root.join("node").join(NODE)),
            "Nodes keep V35's layout"
        );
        assert_eq!(device_dir(root, "../x", "probe", "2"), None);
    }

    /// Nothing the client names reaches the file system: the stored file is
    /// `{id}.pdf` under two checked segments, and the client's name is only
    /// ever displayed.
    #[test]
    fn file_names_and_ids_are_kept_off_the_file_system() {
        assert_eq!(display_file_name(Some("C:\\scans\\cert.pdf")), "cert.pdf");
        assert_eq!(display_file_name(Some("../../etc/passwd")), "passwd");
        assert_eq!(display_file_name(Some("a\"b\u{7}.pdf")), "ab.pdf");
        assert_eq!(display_file_name(Some("   ")), "certificate.pdf");
        assert_eq!(display_file_name(None), "certificate.pdf");
        assert_eq!(
            display_file_name(Some("Kalibrační list.pdf")),
            "Kalibrační list.pdf"
        );

        assert!(is_upload_id(&uuid::Uuid::new_v4().simple().to_string()));
        for bad in ["../x", "ABCDEF0123456789ABCDEF0123456789", "", "0123"] {
            assert!(!is_upload_id(bad), "{bad:?}");
        }
        assert!(safe_segment("AA:BB:CC:DD:EE:01").is_some());
        assert!(safe_segment(NODE).is_some());
        for bad in ["..", "a/b", "", "a b"] {
            assert!(safe_segment(bad).is_none(), "{bad:?}");
        }
    }

    // ---- the alarm -----------------------------------------------------

    fn reading(node_key: &str) -> NodeReadingMsg {
        NodeReadingMsg {
            page_count: None,
            page_index: None,
            message_id: format!("m-{node_key}"),
            dev_eui: node_key.to_string(),
            serial: None,
            fields: [("temperature".to_string(), 4.2)].into_iter().collect(),
            counters: Default::default(),
            events: Vec::new(),
            gateways: Vec::new(),
            rssi: None,
            snr: None,
            fport: None,
            dr: None,
            received_at: "2026-09-25T10:00:00Z".to_string(),
        }
    }

    struct Harness {
        svc: CalibrationService,
        jobs: mpsc::UnboundedReceiver<NotifyJob>,
        nudges: broadcast::Receiver<AlarmNudge>,
        db: Db,
        store: Arc<StateStore>,
        data_dir: String,
        _dir: tempfile::TempDir,
    }

    fn harness() -> Harness {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8").to_string();
        let db = Db::open(&data_dir, None, "test").expect("open db");
        let store = Arc::new(StateStore::new());
        store.apply(IngestEvent::NodeReading {
            hostname: HOST.into(),
            node_key: NODE.into(),
            payload: Box::new(reading(NODE)),
        });
        let (tx, jobs) = mpsc::unbounded_channel();
        let (nudge_tx, nudges) = broadcast::channel(16);
        let svc = CalibrationService::test_instance(db.clone(), &data_dir, Arc::clone(&store))
            .wire(tx, nudge_tx);
        Harness {
            svc,
            jobs,
            nudges,
            db,
            store,
            data_dir,
            _dir: dir,
        }
    }

    /// A portal restart: a fresh service over the same database, files and
    /// live state — nothing in memory survives, the table does.
    fn restart(h: &mut Harness) {
        let (tx, jobs) = mpsc::unbounded_channel();
        let (nudge_tx, nudges) = broadcast::channel(16);
        h.svc = CalibrationService::test_instance(h.db.clone(), &h.data_dir, Arc::clone(&h.store))
            .wire(tx, nudge_tx);
        h.jobs = jobs;
        h.nudges = nudges;
    }

    fn key() -> DeviceKey {
        DeviceKey::new(HOST, CalibrationScope::Node, NODE)
    }

    fn pdf(valid_until: i64) -> Upload {
        Upload {
            file_name: Some("cert.pdf".into()),
            bytes: b"%PDF-1.4\n% test certificate\n%%EOF\n".to_vec(),
            valid_until,
            note: None,
        }
    }

    /// Every calibration row of the test Hub, cleared ones included.
    async fn alarms(h: &Harness) -> Vec<portal_storage::AlarmEvent> {
        h.db.run(|s| {
            s.list_alarms(&AlarmListFilter {
                limit: 100,
                include_cleared: true,
                alarm_type: Some(ALARM_TYPE.to_string()),
                ..Default::default()
            })
        })
        .await
        .expect("list alarms")
        .0
    }

    fn mailed(h: &mut Harness) -> Vec<SystemAlertKind> {
        let mut kinds = Vec::new();
        while let Ok(job) = h.jobs.try_recv() {
            if let NotifyJob::SystemAlerts { events, .. } = job {
                kinds.extend(events.into_iter().map(|e| e.kind));
            }
        }
        kinds
    }

    /// The whole §3 arc (amended 2026-10-13) over one row: an upload inside
    /// the first window opens it at warning and mails the onset; the second
    /// window keeps it a warning, rewrites its sentence and mails the second
    /// warning; the expiry date turns it critical and mails that; every
    /// other pass mails nothing; a new certificate closes it.
    #[tokio::test]
    async fn one_alarm_row_warns_twice_then_expires_then_is_closed_by_an_upload() {
        let mut h = harness();
        let today = now_unix().div_euclid(DAY_S) * DAY_S;
        let expiry = today + 20 * DAY_S;

        let first = h
            .svc
            .upload(&key(), "novak", pdf(expiry))
            .await
            .expect("upload");
        assert_eq!(first.file_name, "cert.pdf");

        let rows = alarms(&h).await;
        assert_eq!(rows.len(), 1, "one row per device");
        let a = &rows[0];
        assert_eq!(a.severity, "warning");
        assert_eq!(a.alarm_type, "CALIBRATION");
        assert_eq!(a.sensor_id, format!("cal:node:{NODE}"));
        assert_eq!(
            a.sensor_name.as_deref(),
            Some(NODE),
            "named like the device"
        );
        assert!(a.cleared_at.is_none());
        assert_eq!((a.value, a.threshold), (None, None));
        assert_eq!(
            a.message.as_deref(),
            Some(format!("Calibration expires on {}", format_date(expiry)).as_str())
        );
        assert_eq!(mailed(&mut h), [SystemAlertKind::CalibrationDue]);
        assert!(h.nudges.try_recv().is_ok(), "the alarm table repaints");

        // A second pass on the same day changes nothing and mails nothing.
        let again = h.svc.check_all(now_unix()).await;
        assert_eq!(again, [(key(), AlarmOutcome::Unchanged)]);
        assert!(mailed(&mut h).is_empty());

        // Thirteen days on: seven left, exactly the second window's edge.
        let outcomes = h.svc.check_all(now_unix() + 13 * DAY_S).await;
        assert!(
            matches!(
                outcomes[0].1,
                AlarmOutcome::Escalated {
                    level: CalibrationLevel::SecondWarning,
                    ..
                }
            ),
            "{outcomes:?}"
        );
        let rows = alarms(&h).await;
        assert_eq!(rows.len(), 1, "moved in place, not a second row");
        assert_eq!(
            rows[0].severity, "warning",
            "the second warning is no alarm"
        );
        assert!(rows[0]
            .message
            .as_deref()
            .is_some_and(|m| m.starts_with("Calibration expires within 7 days")));
        assert_eq!(mailed(&mut h), [SystemAlertKind::CalibrationSecondWarning]);

        // The last valid day: still the second warning, nothing new.
        let outcomes = h.svc.check_all(now_unix() + 19 * DAY_S).await;
        assert_eq!(outcomes[0].1, AlarmOutcome::Unchanged);
        assert!(mailed(&mut h).is_empty());

        // The expiry date itself: critical, and the third mail.
        let outcomes = h.svc.check_all(now_unix() + 20 * DAY_S).await;
        assert!(matches!(
            outcomes[0].1,
            AlarmOutcome::Escalated {
                level: CalibrationLevel::Expired,
                ..
            }
        ));
        let rows = alarms(&h).await;
        assert_eq!(rows[0].severity, "critical");
        assert_eq!(rows[0].to_state.as_deref(), Some("CRITICAL"));
        assert_eq!(
            rows[0].message.as_deref(),
            Some(format!("Calibration expired on {}", format_date(expiry)).as_str())
        );
        assert_eq!(mailed(&mut h), [SystemAlertKind::CalibrationExpired]);

        let outcomes = h.svc.check_all(now_unix() + 40 * DAY_S).await;
        assert_eq!(outcomes[0].1, AlarmOutcome::Unchanged);
        assert!(mailed(&mut h).is_empty(), "expired stays expired, silently");

        // A fresh certificate valid for a year closes the row.
        h.svc
            .upload(&key(), "novak", pdf(today + 365 * DAY_S))
            .await
            .expect("second upload");
        let rows = alarms(&h).await;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].cleared_at.is_some(), "closed by the upload");
        assert!(mailed(&mut h).is_empty());
        assert_eq!(
            h.svc.levels_for(HOST, CalibrationScope::Node).get(NODE),
            Some(&CalibrationLevel::Valid)
        );
    }

    /// Each level's e-mail goes once, and a restart does not send it again:
    /// the open row, not the memory, says which level was already mailed.
    #[tokio::test]
    async fn a_restart_does_not_mail_a_level_again() {
        let mut h = harness();
        let today = now_unix().div_euclid(DAY_S) * DAY_S;
        h.svc
            .upload(&key(), "novak", pdf(today + 5 * DAY_S))
            .await
            .expect("upload");
        assert_eq!(
            mailed(&mut h),
            [SystemAlertKind::CalibrationSecondWarning],
            "an upload straight inside the second window mails that one only"
        );

        restart(&mut h);
        let outcomes = h.svc.check_all(now_unix()).await;
        assert_eq!(outcomes, [(key(), AlarmOutcome::Unchanged)]);
        assert!(mailed(&mut h).is_empty(), "no second-warning mail again");

        let outcomes = h.svc.check_all(now_unix() + 5 * DAY_S).await;
        assert!(matches!(outcomes[0].1, AlarmOutcome::Escalated { .. }));
        assert_eq!(mailed(&mut h), [SystemAlertKind::CalibrationExpired]);

        restart(&mut h);
        h.svc.check_all(now_unix() + 6 * DAY_S).await;
        assert!(mailed(&mut h).is_empty(), "no expiry mail again");
        assert_eq!(alarms(&h).await.len(), 1);
    }

    /// "posunutím data za hranici warning_days" closes the alarm; a date
    /// moved back inside it opens a new one, and a date moved to a calmer
    /// level is an update that mails nothing.
    #[tokio::test]
    async fn moving_the_date_out_of_the_warning_window_closes_the_alarm() {
        let mut h = harness();
        let today = now_unix().div_euclid(DAY_S) * DAY_S;
        h.svc
            .upload(&key(), "novak", pdf(today))
            .await
            .expect("upload");
        let rows = alarms(&h).await;
        assert_eq!(
            rows[0].severity, "critical",
            "an upload expiring today opens critical"
        );
        assert_eq!(mailed(&mut h), [SystemAlertKind::CalibrationExpired]);

        let moved = h
            .svc
            .set_valid_until(&key(), today + 31 * DAY_S)
            .await
            .expect("date change");
        assert!(moved.is_some());
        let rows = alarms(&h).await;
        assert!(
            rows[0].cleared_at.is_some(),
            "31 days out is beyond the window"
        );

        h.svc
            .set_valid_until(&key(), today + 5 * DAY_S)
            .await
            .expect("date change");
        let open: Vec<_> = alarms(&h)
            .await
            .into_iter()
            .filter(|a| a.cleared_at.is_none())
            .collect();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].severity, "warning");
        assert_eq!(mailed(&mut h), [SystemAlertKind::CalibrationSecondWarning]);

        h.svc
            .set_valid_until(&key(), today + 20 * DAY_S)
            .await
            .expect("date change");
        let open: Vec<_> = alarms(&h)
            .await
            .into_iter()
            .filter(|a| a.cleared_at.is_none())
            .collect();
        assert_eq!(open.len(), 1, "back to the first warning, same row");
        assert!(open[0]
            .message
            .as_deref()
            .is_some_and(|m| m.starts_with(WARNING_PREFIX)));
        assert!(mailed(&mut h).is_empty(), "a calmer level mails nothing");
    }

    /// Amendment 2026-10-13: deleting the current certificate hands the
    /// alarm to the previous one standing at once, and deleting the last
    /// one closes it. The file goes, the row stays marked, the download is
    /// gone, a second delete is refused, and a reason is required.
    #[tokio::test]
    async fn deleting_the_current_certificate_recomputes_and_then_closes_the_alarm() {
        let mut h = harness();
        let today = now_unix().div_euclid(DAY_S) * DAY_S;
        let older = h
            .svc
            .upload(&key(), "novak", pdf(today + 20 * DAY_S))
            .await
            .expect("upload");
        let newer = h
            .svc
            .upload(&key(), "novak", pdf(today + 365 * DAY_S))
            .await
            .expect("upload");
        assert!(alarms(&h).await.iter().all(|a| a.cleared_at.is_some()));
        mailed(&mut h);
        let file = |id: &str| h.svc.root.join("node").join(NODE).join(format!("{id}.pdf"));
        assert!(file(&newer.id).is_file());

        for (raw, want) in [("", "required"), ("   \n ", "required")] {
            let err = h
                .svc
                .delete(&key(), &newer.id, "admin1", raw)
                .await
                .expect_err("no reason, no delete");
            assert!(
                matches!(err, DeleteError::MessageRequired),
                "{want}: {err:?}"
            );
        }
        let err = h
            .svc
            .delete(
                &key(),
                &newer.id,
                "admin1",
                &"x".repeat(MAX_DELETE_MESSAGE_CHARS + 1),
            )
            .await
            .expect_err("too long");
        assert!(matches!(err, DeleteError::MessageTooLong));
        assert!(
            file(&newer.id).is_file(),
            "a refused delete leaves the file"
        );

        let deleted = h
            .svc
            .delete(&key(), &newer.id, "admin1", "  wrong device  ")
            .await
            .expect("delete");
        assert!(deleted.was_current);
        assert_eq!(deleted.row.delete_message.as_deref(), Some("wrong device"));
        assert_eq!(deleted.row.deleted_by.as_deref(), Some("admin1"));
        assert!(!file(&newer.id).exists(), "the PDF is gone");
        assert!(h.svc.certificate(&key(), &newer.id).await.is_none());
        assert_eq!(h.svc.history(&key()).await.expect("history").len(), 2);

        let open: Vec<_> = alarms(&h)
            .await
            .into_iter()
            .filter(|a| a.cleared_at.is_none())
            .collect();
        assert_eq!(open.len(), 1, "the older certificate is current again");
        assert_eq!(open[0].severity, "warning");
        assert_eq!(mailed(&mut h), [SystemAlertKind::CalibrationDue]);
        assert_eq!(
            h.svc.levels_for(HOST, CalibrationScope::Node).get(NODE),
            Some(&CalibrationLevel::Warning)
        );

        assert!(matches!(
            h.svc.delete(&key(), &newer.id, "admin1", "again").await,
            Err(DeleteError::NotFound)
        ));
        let other = DeviceKey::new(HOST, CalibrationScope::Beacon, "AA:BB:CC:DD:EE:01");
        assert!(matches!(
            h.svc.delete(&other, &older.id, "admin1", "x").await,
            Err(DeleteError::NotFound)
        ));

        let deleted = h
            .svc
            .delete(&key(), &older.id, "admin1", "lab withdrew it")
            .await
            .expect("delete");
        assert!(deleted.was_current);
        assert!(
            alarms(&h).await.iter().all(|a| a.cleared_at.is_some()),
            "no certificate standing, no alarm"
        );
        assert!(mailed(&mut h).is_empty());
        assert!(!h
            .svc
            .levels_for(HOST, CalibrationScope::Node)
            .contains_key(NODE));
    }

    /// Deleting an older certificate leaves the current one and its alarm
    /// alone; a sweep closes a reminder whose device has nothing standing,
    /// even one the delete's own recheck did not get to.
    #[tokio::test]
    async fn an_old_delete_keeps_the_alarm_and_the_sweep_closes_an_orphan() {
        let mut h = harness();
        let today = now_unix().div_euclid(DAY_S) * DAY_S;
        let older = h
            .svc
            .upload(&key(), "novak", pdf(today + 400 * DAY_S))
            .await
            .expect("upload");
        h.svc
            .upload(&key(), "novak", pdf(today + 3 * DAY_S))
            .await
            .expect("upload");
        mailed(&mut h);
        let deleted = h
            .svc
            .delete(&key(), &older.id, "admin1", "duplicate")
            .await
            .expect("delete");
        assert!(!deleted.was_current);
        let open: Vec<_> = alarms(&h)
            .await
            .into_iter()
            .filter(|a| a.cleared_at.is_none())
            .collect();
        assert_eq!(open.len(), 1, "the current certificate still alarms");
        assert!(mailed(&mut h).is_empty());

        // The current row marked deleted behind the service's back: the
        // next sweep finds the reminder with nothing standing and closes it.
        let current = h.svc.history(&key()).await.expect("history")[0].id.clone();
        h.db.write(move |s| s.mark_device_calibration_deleted(&current, "admin1", "x"))
            .await
            .expect("mark");
        let outcomes = h.svc.check_all(now_unix()).await;
        assert!(
            outcomes
                .iter()
                .any(|(k, o)| *k == key() && matches!(o, AlarmOutcome::Cleared { .. })),
            "{outcomes:?}"
        );
        assert!(alarms(&h).await.iter().all(|a| a.cleared_at.is_some()));
    }

    /// The sweep writes alarms only for devices the snapshot holds (the
    /// module doc's restart argument), and a removal through the portal
    /// closes the one it left open.
    #[tokio::test]
    async fn only_known_devices_get_an_alarm_and_a_removal_closes_it() {
        let h = harness();
        let today = now_unix().div_euclid(DAY_S) * DAY_S;
        let ghost = DeviceKey::new(HOST, CalibrationScope::Node, "70b3d5e05001ffff");
        h.svc
            .upload(&ghost, "novak", pdf(today))
            .await
            .expect("a row can exist for a device the snapshot lost");
        assert!(
            alarms(&h).await.is_empty(),
            "no alarm for an unknown device"
        );
        assert_eq!(
            h.svc
                .levels_for(HOST, CalibrationScope::Node)
                .get("70b3d5e05001ffff"),
            Some(&CalibrationLevel::Expired),
            "the level is still known"
        );

        h.svc
            .upload(&key(), "novak", pdf(today))
            .await
            .expect("upload");
        assert_eq!(alarms(&h).await.len(), 1);
        h.svc.forget_device(&key()).await;
        assert!(alarms(&h).await[0].cleared_at.is_some());
        assert!(!h
            .svc
            .levels_for(HOST, CalibrationScope::Node)
            .contains_key(NODE));
    }

    /// The upload's own refusals, before anything touches the disk.
    #[tokio::test]
    async fn an_upload_refuses_what_is_not_a_small_pdf() {
        let h = harness();
        let mut not_pdf = pdf(OCT_31);
        not_pdf.bytes = b"GIF89a".to_vec();
        assert!(matches!(
            h.svc.upload(&key(), "novak", not_pdf).await,
            Err(UploadError::NotPdf)
        ));

        let mut empty = pdf(OCT_31);
        empty.bytes.clear();
        assert!(matches!(
            h.svc.upload(&key(), "novak", empty).await,
            Err(UploadError::NoFile)
        ));

        let mut big = pdf(OCT_31);
        big.bytes.resize(MAX_PDF_BYTES + 1, b' ');
        assert!(matches!(
            h.svc.upload(&key(), "novak", big).await,
            Err(UploadError::TooLarge)
        ));

        let mut chatty = pdf(OCT_31);
        chatty.note = Some("x".repeat(MAX_NOTE_CHARS + 1));
        assert!(matches!(
            h.svc.upload(&key(), "novak", chatty).await,
            Err(UploadError::NoteTooLong)
        ));

        assert!(h.svc.history(&key()).await.expect("history").is_empty());
    }

    /// §4's cap: the 51st certificate is refused, and the stored bytes are
    /// the uploaded ones, where the row says they are.
    #[tokio::test]
    async fn the_fifty_first_certificate_is_refused_and_files_land_where_rows_say() {
        let h = harness();
        let row = h
            .svc
            .upload(&key(), "novak", pdf(OCT_31))
            .await
            .expect("upload");
        let (back, bytes) = h
            .svc
            .certificate(&key(), &row.id)
            .await
            .expect("the file is served");
        assert_eq!(back.id, row.id);
        assert_eq!(bytes, pdf(OCT_31).bytes);
        assert_eq!(
            row.sha256,
            hex(&Sha256::digest(&pdf(OCT_31).bytes)),
            "the digest is of the stored bytes"
        );
        let on_disk = h
            .svc
            .root
            .join("node")
            .join(NODE)
            .join(format!("{}.pdf", row.id));
        assert!(on_disk.is_file(), "{}", on_disk.display());
        let other = DeviceKey::new(HOST, CalibrationScope::Beacon, "AA:BB:CC:DD:EE:01");
        assert!(
            h.svc.certificate(&other, &row.id).await.is_none(),
            "one device's id does not open another's file"
        );

        for _ in 1..MAX_PER_DEVICE {
            h.svc
                .upload(&key(), "novak", pdf(OCT_31))
                .await
                .expect("upload");
        }
        assert!(matches!(
            h.svc.upload(&key(), "novak", pdf(OCT_31)).await,
            Err(UploadError::LimitReached)
        ));
    }

    #[test]
    fn every_upload_refusal_and_level_has_a_message_in_both_languages() {
        let mut keys: Vec<String> = [
            UploadError::NoFile,
            UploadError::NotPdf,
            UploadError::TooLarge,
            UploadError::LimitReached,
            UploadError::NoteTooLong,
            UploadError::BadIdentity,
        ]
        .iter()
        .map(|e| e.message_key().to_string())
        .collect();
        for err in [
            DeleteError::MessageRequired,
            DeleteError::MessageTooLong,
            DeleteError::NotFound,
            DeleteError::Io(std::io::Error::other("x")),
        ] {
            keys.push(err.message_key().to_string());
        }
        for level in [
            CalibrationLevel::Valid,
            CalibrationLevel::Warning,
            CalibrationLevel::SecondWarning,
            CalibrationLevel::Expired,
        ] {
            keys.push(format!("monitoring.calibration.state_{}", level.as_str()));
            if let Some(kind) = level.mail_kind() {
                for group in ["msg", "title"] {
                    keys.push(format!("alarms.email.system.{group}.{}", kind.as_str()));
                }
            }
        }
        keys.push("monitoring.calibration.state_second_warning_one".to_string());
        keys.push("alarms.email.system.msg.calibration_second_warning_one".to_string());
        for days in ["one", "few", "many"] {
            keys.push(format!("monitoring.calibration.left_{days}"));
        }
        // `ui.alarm_type_label` lowercases the stored type into this key.
        keys.push(format!("alarms.type.{}", ALARM_TYPE.to_lowercase()));
        for key in keys {
            for lang in crate::i18n::LANGS {
                assert_ne!(
                    crate::i18n::lookup(lang, &key),
                    key,
                    "{key} is missing from the {lang} catalogue"
                );
            }
        }
    }
}
