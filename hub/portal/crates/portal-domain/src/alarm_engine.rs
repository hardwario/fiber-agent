//! The alarm engine: pure reducers from one decoded MQTT-shaped input to a
//! list of [`AlarmAction`]s, driven entirely by the caller-supplied
//! [`ActiveLookup`] snapshot. No I/O, no clock, no persistence — the bin's
//! ingest layer (M1+) owns applying the actions and feeding notifications
//! to the email/webhook senders.
//!
//! ## Behavior matrix (normative source: plan/v1/viewer/mqtt.md
//! `alarms/events` + `system/info` + `lorawan/sensors` + `eye/sensors`,
//! plan/v1/viewer/ws.md anti-spam)
//!
//! | Input | Active row | Action |
//! |---|---|---|
//! | wired `to_state=NORMAL` | none | Nothing |
//! | wired `to_state=NORMAL` | any | Clear + Notify(Cleared) |
//! | wired `to_state=WARNING\|CRITICAL` | none | Open(HIGH/LOW inferred) + Notify(Activated) |
//! | wired `to_state=WARNING\|CRITICAL` | same severity | Nothing (anti-spam duplicate) |
//! | wired `to_state=WARNING`, active CRITICAL (same kind) | HIGH/LOW | UpdateSeverity(Warning) |
//! | wired `to_state=CRITICAL`, active WARNING (same kind) | HIGH/LOW | UpdateSeverity(Critical) (+ Notify(Escalated) on the first reach of critical) |
//! | wired `to_state=DISCONNECTED\|RECONNECTING` | none | Open(DISCONNECT) + Notify(Activated) |
//! | wired `to_state=DISCONNECTED\|RECONNECTING`, active DISCONNECT, same to_state | DISCONNECT | Nothing (duplicate) |
//! | wired `to_state=DISCONNECTED\|RECONNECTING`, active DISCONNECT, different to_state | DISCONNECT | UpdateStateInPlace |
//! | wired transition where the active row's *kind* doesn't match the new state's kind (e.g. HIGH/LOW active but now DISCONNECTED, or DISCONNECT active but now WARNING) | any | Clear(old) + Open(new) + Notify(Activated) — see "kind supersede" below |
//! | wired `alarm_type_hint` present | per [`ConnectivityKind`] | same as `on_connectivity`, keyed `sys:*` |
//! | wired snapshot (`sensors/aggregated`) NORMAL, samples > 0, window starts after `triggered_at` | HIGH/LOW | Clear(message) + Notify(Cleared) — see [`on_wired_snapshot`] |
//! | wired snapshot anything else (WARNING/CRITICAL/connectivity state, empty or older window) | any | Nothing |
//! | wired snapshot NORMAL | DISCONNECT | Nothing (a value verdict does not close a connectivity row) |
//! | connectivity `dc_connected=false` | none (`sys:power`) | Open(POWER_DISCONNECT, critical) + Notify |
//! | connectivity `dc_connected=false` | active | Nothing (one-shot) |
//! | connectivity `dc_connected=true` | active | Clear + Notify |
//! | connectivity `dc_connected=true` | none | Nothing |
//! | connectivity `wifi\|ethernet_connected=false` | none | Open(WIFI\|ETHERNET_DISCONNECT, warning) + Notify — `persist.rs` passes `false` only on a true→false edge it saw (UX-058) |
//! | connectivity `wifi\|ethernet_connected=false` | active | Nothing (one-shot, no re-raise) |
//! | connectivity `wifi\|ethernet_connected=true` | active | Clear + Notify |
//! | connectivity `concentrator_present=false` (lorawan/dual, past grace) | none (`sys:concentrator`) | Open(CONCENTRATOR_MISSING, critical) + Notify |
//! | connectivity `concentrator_present=false` | active | Nothing (one-shot) |
//! | connectivity `concentrator_present=true` | active | Clear + Notify |
//! | connectivity `concentrator_present=None` (off/p2p, unknown, or in grace) | any | Nothing |
//! | connectivity `online=false` (Hub `status` offline, an online→offline edge `persist.rs` saw) | none (`sys:online`) | Open(HUB_OFFLINE, critical) + Notify — plan/decisions.md #30a |
//! | connectivity `online=false` | active | Nothing (one-shot) |
//! | connectivity `online=true` | active | Clear + Notify |
//! | field state NORMAL | none | Nothing |
//! | field state NORMAL | active | Clear + Notify |
//! | field state WARNING\|CRITICAL | none | Open(HIGH/LOW inferred) + Notify |
//! | field state same severity as active | active | Nothing |
//! | field state severity change | active | UpdateSeverity (+ Notify(Escalated) on the first reach of critical) |
//! | native fPort-3 `edge=raise` | none | Open(HIGH/LOW from `side`) + Notify, carries `dedup_key` |
//! | native fPort-3 `edge=raise` | active | Nothing (idempotent duplicate) |
//! | native fPort-3 `edge=clear` | active | Clear + Notify |
//! | native fPort-3 `edge=clear` | none | Nothing |
//!
//! ### "Kind supersede" — a deliberate extension beyond the literal v1 spec
//!
//! mqtt.md's wired-probe prose reads "`to_state != NORMAL` → dedup/severity-change
//! logic → create_alarm" and separately calls out that
//! DISCONNECTED<->RECONNECTING updates one row in place. Taken completely
//! literally, "→DISCONNECTED opens DISCONNECT" would open a *second* active
//! row when a HIGH/LOW alarm is already active for the same sensor and the
//! probe then reports disconnected (or the reverse: reconnects straight
//! into a threshold alarm, skipping NORMAL) — violating ws.md's
//! one-active-alarm-per-`(hostname, sensor_id)` anti-spam invariant, which
//! this work order's acceptance criteria explicitly re-assert as a
//! property ("no action ever opens a second active row for the same
//! (hostname, sensor_id) given a truthful ActiveLookup"). Since the two
//! requirements conflict under a literal reading, this implementation
//! closes the old row first: `[Clear(old), Open(new)]`. The property holds
//! against the *resulting* state once both actions are applied, which is
//! how the invariant is interpreted and tested here.
//!
//! ### Other documented assumptions
//!
//! - Plain wired `DISCONNECT` alarms (not POWER/WIFI/ETHERNET) default to
//!   `Severity::Critical` — mqtt.md doesn't spell out a severity for this
//!   case, but a physically disconnected probe is treated as a fault at
//!   least as serious as a threshold breach.
//! - Native fPort-3 alarms default to `Severity::Warning` uniformly — the
//!   documented wire shape (`{slot, source, quantity, edge, alarm_type|side,
//!   value, base_time, rel_s, ts}`) carries no separate warning/critical
//!   level, unlike the wired/field paths.
//! - Missing/unknown thresholds (or a missing value) default to
//!   `AlarmType::High`, per this work order's own "unknown thresholds ->
//!   HIGH default" instruction; applied uniformly to wired, field, and
//!   native inference.
//! - `UpdateSeverity` re-infers `alarm_type`/`threshold` at the new
//!   severity's bounds and carries the new `to_state`, so an escalated row
//!   doesn't keep describing the level it opened at.
//!   The storage layer also keeps the worst level the row ever reached
//!   (`max_severity`, plan/decisions.md #24), which this rewrite never
//!   lowers — a deviation from v1, which forgot it.
//! - `UpdateStateInPlace` never carries a `Notify`, and neither does
//!   `UpdateSeverity` — with one exception: the first time an open alarm
//!   reaches critical it is followed by `Notify(Escalated)`
//!   (plan/decisions.md #26; v1 sent nothing, so a warning that went
//!   critical never told anyone). Falling back to warning, and climbing to
//!   critical again, stay silent: one notify per open, per first
//!   escalation and per close, not one per wobble across the bound. That
//!   first escalation also clears the row's acknowledgement
//!   (`clears_acknowledgement`, plan/decisions.md #28c).
//! - `Notify(Cleared)`'s `threshold` is always `None`: [`ActiveAlarm`] (as
//!   specified) only exposes `{id, severity, alarm_type, to_state}`, not
//!   the originally-crossed value. A notifier that wants it must read the
//!   stored row (which does have `threshold`, per `AlarmEventDB`).

use crate::types::{
    ActiveAlarm, ActiveLookup, AlarmAction, AlarmSource, AlarmType, ConnectivityKind,
    ConnectivitySnapshot, Edge, FieldSource, FieldStates, NativeEvent, NewAlarm, NotifyKind,
    NotifyPayload, Severity, ThresholdBounds, WireState, WiredSnapshot, WiredTransition,
    NODE_SENSOR_ID_TAG,
};

// ---------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------

/// Unknown thresholds/value -> `High` (documented default). Otherwise picks
/// the side whose bound the value violates, returning the crossed bound.
fn infer_high_low(
    value: Option<f64>,
    severity: Severity,
    thresholds: Option<&ThresholdBounds>,
) -> (AlarmType, Option<f64>) {
    let (Some(v), Some(t)) = (value, thresholds) else {
        return (AlarmType::High, None);
    };
    let (low, high) = match severity {
        Severity::Critical => (t.critical_low, t.critical_high),
        Severity::Warning => (t.warning_low, t.warning_high),
    };
    if let Some(lo) = low {
        if v <= lo {
            return (AlarmType::Low, Some(lo));
        }
    }
    if let Some(hi) = high {
        if v >= hi {
            return (AlarmType::High, Some(hi));
        }
    }
    (AlarmType::High, None)
}

fn notify(kind: NotifyKind) -> AlarmAction {
    AlarmAction::Notify(kind)
}

/// The identifying fields a `Notify` needs beyond what [`ActiveAlarm`]/
/// [`NewAlarm`] already carry — bundled so `clear_with_notify`/
/// `open_with_notify` stay under clippy's argument-count lint.
struct NotifyContext {
    source: AlarmSource,
    sensor_name: Option<String>,
    location: Option<String>,
    line: Option<i64>,
    /// Why a clear happened, when the clear path is not the obvious one.
    /// `clear_with_notify` stores it on the row and in the notify payload.
    message: Option<String>,
}

impl NotifyContext {
    fn new(source: AlarmSource) -> Self {
        Self {
            source,
            sensor_name: None,
            location: None,
            line: None,
            message: None,
        }
    }

    fn with_message(mut self, message: &str) -> Self {
        self.message = Some(message.to_string());
        self
    }

    fn with_sensor_name(mut self, name: Option<String>) -> Self {
        self.sensor_name = name;
        self
    }

    fn with_location(mut self, location: Option<String>) -> Self {
        self.location = location;
        self
    }

    fn with_line(mut self, line: i64) -> Self {
        self.line = Some(line);
        self
    }
}

/// `[Clear, Notify(Cleared)]` — shared tail for every reducer's clear path.
fn clear_with_notify(
    active: &ActiveAlarm,
    hostname: &str,
    sensor_id: &str,
    ctx: NotifyContext,
    value: Option<f64>,
    ts_iso: &str,
) -> Vec<AlarmAction> {
    vec![
        AlarmAction::Clear {
            id: active.id.clone(),
            cleared_at_iso: ts_iso.to_string(),
            message: ctx.message.clone(),
        },
        notify(NotifyKind::Cleared(NotifyPayload {
            severity: active.severity,
            alarm_type: active.alarm_type,
            source: ctx.source,
            value,
            threshold: None,
            message: ctx.message,
            hostname: hostname.to_string(),
            device_label: None,
            sensor_name: ctx.sensor_name,
            sensor_id: sensor_id.to_string(),
            line: ctx.line,
            location: ctx.location,
            triggered_at: ts_iso.to_string(),
        })),
    ]
}

/// The level a row moves to, and what an Open at that level would record.
struct SeverityChange {
    severity: Severity,
    alarm_type: AlarmType,
    to_state: Option<WireState>,
    value: Option<f64>,
    threshold: Option<f64>,
}

/// `[UpdateSeverity]`, plus `Notify(Escalated)` the first time the row
/// reaches critical (plan/decisions.md #26) — shared by every reducer that
/// moves an open alarm between warning and critical. A fall back to warning,
/// and a second climb to critical, stay silent: the operator has already
/// been told the worst. The same first escalation clears the row's
/// acknowledgement (plan/decisions.md #28c): it was given to a warning.
fn update_severity(
    active: &ActiveAlarm,
    hostname: &str,
    sensor_id: &str,
    ctx: NotifyContext,
    change: SeverityChange,
    ts_iso: &str,
) -> Vec<AlarmAction> {
    let escalated =
        change.severity == Severity::Critical && active.max_severity != Severity::Critical;
    let mut actions = vec![AlarmAction::UpdateSeverity {
        id: active.id.clone(),
        severity: change.severity,
        alarm_type: change.alarm_type,
        to_state: change.to_state,
        value: change.value,
        threshold: change.threshold,
        message: None,
        clears_acknowledgement: escalated,
    }];
    if escalated {
        actions.push(notify(NotifyKind::Escalated(NotifyPayload {
            severity: change.severity,
            alarm_type: change.alarm_type,
            source: ctx.source,
            value: change.value,
            threshold: change.threshold,
            message: None,
            hostname: hostname.to_string(),
            device_label: None,
            sensor_name: ctx.sensor_name,
            sensor_id: sensor_id.to_string(),
            line: ctx.line,
            location: ctx.location,
            triggered_at: ts_iso.to_string(),
        })));
    }
    actions
}

/// `[Open, Notify(Activated)]` — shared head for every reducer's open path.
fn open_with_notify(alarm: NewAlarm, ctx: NotifyContext) -> Vec<AlarmAction> {
    let payload = NotifyPayload {
        severity: alarm.severity,
        alarm_type: alarm.alarm_type,
        source: ctx.source,
        value: alarm.value,
        threshold: alarm.threshold,
        message: alarm.message.clone(),
        hostname: alarm.hostname.clone(),
        device_label: None,
        sensor_name: alarm.sensor_name.clone(),
        sensor_id: alarm.sensor_id.clone(),
        line: ctx.line,
        location: ctx.location,
        triggered_at: alarm.triggered_at.clone(),
    };
    vec![
        AlarmAction::Open(alarm),
        notify(NotifyKind::Activated(payload)),
    ]
}

// ---------------------------------------------------------------------
// Wired probes — `{prefix}/{host}/alarms/events`
// ---------------------------------------------------------------------

fn wired_sensor_id(line: i64) -> String {
    format!("line-{line}")
}

fn wired_ctx(t: &WiredTransition) -> NotifyContext {
    NotifyContext::new(AlarmSource::Fiber)
        .with_line(t.line)
        .with_sensor_name(t.name.clone())
        .with_location(t.location.clone())
}

fn open_threshold_alarm(
    t: &WiredTransition,
    severity: Severity,
    sensor_id: &str,
) -> Vec<AlarmAction> {
    let (alarm_type, threshold) = infer_high_low(t.temperature, severity, t.thresholds.as_ref());
    let alarm = NewAlarm {
        hostname: t.hostname.clone(),
        sensor_id: sensor_id.to_string(),
        sensor_name: t.name.clone(),
        alarm_type,
        severity,
        from_state: Some(t.from_state),
        to_state: Some(t.to_state),
        message: None,
        value: t.temperature,
        threshold,
        triggered_at: t.ts_iso.clone(),
        dedup_key: None,
    };
    open_with_notify(alarm, wired_ctx(t))
}

fn open_disconnect_alarm(t: &WiredTransition, sensor_id: &str) -> Vec<AlarmAction> {
    let alarm = NewAlarm {
        hostname: t.hostname.clone(),
        sensor_id: sensor_id.to_string(),
        sensor_name: t.name.clone(),
        alarm_type: AlarmType::Disconnect,
        // See module docs: v1 doesn't spell out a severity for plain wired
        // DISCONNECT; treated as critical like POWER_DISCONNECT.
        severity: Severity::Critical,
        from_state: Some(t.from_state),
        to_state: Some(t.to_state),
        message: None,
        value: t.temperature,
        threshold: None,
        triggered_at: t.ts_iso.clone(),
        dedup_key: None,
    };
    open_with_notify(alarm, wired_ctx(t))
}

/// Reduces one `{prefix}/{host}/alarms/events` transition. See the module doc
/// for the full behavior matrix.
pub fn on_wired_transition(active: &dyn ActiveLookup, t: &WiredTransition) -> Vec<AlarmAction> {
    let sensor_id = wired_sensor_id(t.line);

    if let Some(kind) = t.alarm_type_hint {
        return connectivity_transition(
            active,
            &t.hostname,
            kind,
            t.to_state == WireState::Normal,
            &t.ts_iso,
        );
    }

    let existing = active.active(&t.hostname, &sensor_id);

    match t.to_state {
        WireState::Normal => match existing {
            Some(a) => clear_with_notify(
                &a,
                &t.hostname,
                &sensor_id,
                wired_ctx(t),
                t.temperature,
                &t.ts_iso,
            ),
            None => Vec::new(),
        },
        WireState::NeverConnected => Vec::new(),
        WireState::Warning | WireState::Critical => {
            // Match arm guarantees Warning/Critical, so this is always Some.
            let severity = if t.to_state == WireState::Critical {
                Severity::Critical
            } else {
                Severity::Warning
            };
            match existing {
                None => open_threshold_alarm(t, severity, &sensor_id),
                Some(a) if a.alarm_type == AlarmType::Disconnect => {
                    let mut actions = clear_with_notify(
                        &a,
                        &t.hostname,
                        &sensor_id,
                        wired_ctx(t),
                        t.temperature,
                        &t.ts_iso,
                    );
                    actions.extend(open_threshold_alarm(t, severity, &sensor_id));
                    actions
                }
                Some(a) if a.severity == severity => Vec::new(),
                Some(a) => {
                    // Re-inferred at the new severity's bounds, like an Open
                    // would: the row's stored type/threshold belong to the
                    // level it opened at.
                    let (alarm_type, threshold) =
                        infer_high_low(t.temperature, severity, t.thresholds.as_ref());
                    update_severity(
                        &a,
                        &t.hostname,
                        &sensor_id,
                        wired_ctx(t),
                        SeverityChange {
                            severity,
                            alarm_type,
                            to_state: Some(t.to_state),
                            value: t.temperature,
                            threshold,
                        },
                        &t.ts_iso,
                    )
                }
            }
        }
        WireState::Disconnected | WireState::Reconnecting => match existing {
            None => open_disconnect_alarm(t, &sensor_id),
            Some(a) if a.alarm_type == AlarmType::Disconnect => {
                if a.to_state == Some(t.to_state) {
                    Vec::new()
                } else {
                    vec![AlarmAction::UpdateStateInPlace {
                        id: a.id,
                        to_state: t.to_state,
                        message: None,
                    }]
                }
            }
            Some(a) => {
                let mut actions = clear_with_notify(
                    &a,
                    &t.hostname,
                    &sensor_id,
                    wired_ctx(t),
                    t.temperature,
                    &t.ts_iso,
                );
                actions.extend(open_disconnect_alarm(t, &sensor_id));
                actions
            }
        },
    }
}

/// The row `message` a reconcile clear leaves behind, so the detail page and
/// the cleared notification tell it apart from a transition clear.
pub const WIRED_RECONCILE_MESSAGE: &str =
    "Cleared from the Hub's reported state: the transition to NORMAL was not received.";

/// What follows [`WIRED_RECONCILE_MESSAGE`] in the row message since #28b,
/// before the last NORMAL window's start (RFC 3339) and a closing period.
const WIRED_RECONCILE_SINCE: &str = " Last NORMAL window started ";

/// Whether a row `message` says the row was closed from the Hub's reported
/// state, with or without the window start (rows from before #28b have none).
pub fn is_wired_reconcile(message: &str) -> bool {
    message.starts_with(WIRED_RECONCILE_MESSAGE)
}

/// The last NORMAL window's start a reconcile clear wrote into the row
/// `message`; `None` for any other message and for rows from before #28b.
pub fn wired_reconcile_last_normal(message: &str) -> Option<&str> {
    message
        .strip_prefix(WIRED_RECONCILE_MESSAGE)?
        .strip_prefix(WIRED_RECONCILE_SINCE)?
        .strip_suffix('.')
}

/// Reconciles one line of a `{prefix}/{host}/sensors/aggregated` publish with
/// the line's active HIGH/LOW row (`plan/decisions.md` #27).
///
/// Wired rows open and close only on `alarms/events` transitions, which are
/// not retained. A missed NORMAL (portal down, broker restart, a Hub that
/// rebooted and came up already NORMAL) left the row open for good while the
/// card, fed by this publish, said all was well. This reducer closes that gap
/// with the same `[Clear, Notify(Cleared)]` a NORMAL transition produces, plus
/// [`WIRED_RECONCILE_MESSAGE`] on the row. It only ever clears: opening and
/// severity changes stay with the transition stream, so the snapshot can not
/// add a notification the transitions would not have sent.
///
/// All of these must hold, otherwise it returns nothing:
/// - `state` is NORMAL. The Hub reports the window's *most severe* sample
///   state, so NORMAL means no sample in the window was WARNING, CRITICAL,
///   DISCONNECTED or RECONNECTING. A single stray NORMAL sample inside an
///   alarm can not produce it, which is what keeps a flapping value from
///   closing a live alarm.
/// - `sample_count > 0`. An empty window measured nothing and its state
///   falls through to NORMAL on the Hub (and on the portal's serde default).
/// - The active row is HIGH or LOW. A DISCONNECT row is about the probe's
///   connection, and a value verdict is no answer to it; its own
///   transitions close it.
/// - The window started strictly after the row's `triggered_at`, both on the
///   Hub's clock. This rejects a stale snapshot: the Hub buffers up to 100
///   windows while offline and replays them later, and a window from before
///   the alarm (or one delivered after a newer alarm opened) says nothing
///   about this row. An unparseable `triggered_at` also returns nothing.
///
/// `cleared_at` is the portal's time as it closes the row (`now_iso`,
/// decisions.md #28b, amending #27's window start): the row records when the
/// portal knew. The window start, the latest moment the probe is known
/// NORMAL, follows [`WIRED_RECONCILE_MESSAGE`] in the message
/// ([`wired_reconcile_last_normal`] reads it back).
pub fn on_wired_snapshot(active: &dyn ActiveLookup, s: &WiredSnapshot) -> Vec<AlarmAction> {
    if s.state != WireState::Normal || s.sample_count <= 0 {
        return Vec::new();
    }
    let sensor_id = wired_sensor_id(s.line);
    let Some(a) = active.active(&s.hostname, &sensor_id) else {
        return Vec::new();
    };
    if !matches!(a.alarm_type, AlarmType::High | AlarmType::Low) {
        return Vec::new();
    }
    // No parsable trigger time, or a window that did not start after it:
    // the snapshot cannot speak for this alarm.
    if a.triggered_at_unix.is_none_or(|t| s.window_start_unix <= t) {
        return Vec::new();
    }
    let message = format!(
        "{WIRED_RECONCILE_MESSAGE}{WIRED_RECONCILE_SINCE}{}.",
        s.window_start_iso
    );
    let ctx = NotifyContext::new(AlarmSource::Fiber)
        .with_line(s.line)
        .with_sensor_name(s.name.clone())
        .with_location(s.location.clone())
        .with_message(&message);
    clear_with_notify(&a, &s.hostname, &sensor_id, ctx, s.temperature, &s.now_iso)
}

// ---------------------------------------------------------------------
// Connectivity — `{prefix}/{host}/system/info`
// ---------------------------------------------------------------------

fn connectivity_transition(
    active: &dyn ActiveLookup,
    hostname: &str,
    kind: ConnectivityKind,
    connected: bool,
    ts_iso: &str,
) -> Vec<AlarmAction> {
    let sensor_id = kind.sensor_id();
    let existing = active.active(hostname, sensor_id);
    if connected {
        match existing {
            Some(a) => clear_with_notify(
                &a,
                hostname,
                sensor_id,
                NotifyContext::new(AlarmSource::System),
                None,
                ts_iso,
            ),
            None => Vec::new(),
        }
    } else {
        match existing {
            // One-shot: already open, don't re-raise while active.
            Some(_) => Vec::new(),
            None => {
                let alarm = NewAlarm {
                    hostname: hostname.to_string(),
                    sensor_id: sensor_id.to_string(),
                    sensor_name: None,
                    alarm_type: kind.alarm_type(),
                    severity: kind.severity(),
                    from_state: None,
                    to_state: None,
                    message: None,
                    value: None,
                    threshold: None,
                    triggered_at: ts_iso.to_string(),
                    dedup_key: None,
                };
                open_with_notify(alarm, NotifyContext::new(AlarmSource::System))
            }
        }
    }
}

/// Reduces one `{prefix}/{host}/system/info` publish's connectivity leaves.
/// Each of `dc_connected`/`wifi_connected`/`ethernet_connected` is handled
/// independently; `None` (leaf absent/unknown) produces no action.
pub fn on_connectivity(
    active: &dyn ActiveLookup,
    snap: &ConnectivitySnapshot,
    ts_iso: &str,
) -> Vec<AlarmAction> {
    let mut actions = Vec::new();
    if let Some(connected) = snap.dc_connected {
        actions.extend(connectivity_transition(
            active,
            &snap.hostname,
            ConnectivityKind::Power,
            connected,
            ts_iso,
        ));
    }
    if let Some(connected) = snap.wifi_connected {
        actions.extend(connectivity_transition(
            active,
            &snap.hostname,
            ConnectivityKind::Wifi,
            connected,
            ts_iso,
        ));
    }
    if let Some(connected) = snap.ethernet_connected {
        actions.extend(connectivity_transition(
            active,
            &snap.hostname,
            ConnectivityKind::Ethernet,
            connected,
            ts_iso,
        ));
    }
    // Concentrator presence — a one-shot exactly like `dc_connected`, but the
    // ingest layer only sets `Some(_)` when the Hub is in a LoRaWAN-serving mode
    // and the boot grace has elapsed (see `ConnectivitySnapshot`); `None` here
    // means "does not apply", so an `off`/`p2p` Hub never raises it.
    if let Some(present) = snap.concentrator_present {
        actions.extend(connectivity_transition(
            active,
            &snap.hostname,
            ConnectivityKind::Concentrator,
            present,
            ts_iso,
        ));
    }
    // The Hub's own status (decisions.md #30a): the ingest layer passes
    // `false` only on an online→offline edge it saw, like the link flags.
    if let Some(online) = snap.online {
        actions.extend(connectivity_transition(
            active,
            &snap.hostname,
            ConnectivityKind::Online,
            online,
            ts_iso,
        ));
    }
    // The radio-diagnostics pair — the same one-shot shape; the ingest layer does
    // the five-minute debounce and the mode gating before it sets `Some(_)`.
    if let Some(reachable) = snap.northbridge_reachable {
        actions.extend(connectivity_transition(
            active,
            &snap.hostname,
            ConnectivityKind::Northbridge,
            reachable,
            ts_iso,
        ));
    }
    if let Some(running) = snap.chirpstack_running {
        actions.extend(connectivity_transition(
            active,
            &snap.hostname,
            ConnectivityKind::Chirpstack,
            running,
            ts_iso,
        ));
    }
    // The Northbridge firmware update (northbridge_update.md §3.5): `failed` is
    // already the end of a retry budget, so no debounce.
    if let Some(ok) = snap.northbridge_update_ok {
        actions.extend(connectivity_transition(
            active,
            &snap.hostname,
            ConnectivityKind::NorthbridgeUpdate,
            ok,
            ts_iso,
        ));
    }
    // The Southbridge firmware update (southbridge_update.md §3.7): `failed`
    // is the end of an update, so no debounce either.
    if let Some(ok) = snap.southbridge_update_ok {
        actions.extend(connectivity_transition(
            active,
            &snap.hostname,
            ConnectivityKind::SouthbridgeUpdate,
            ok,
            ts_iso,
        ));
    }
    actions
}

// ---------------------------------------------------------------------
// Field states — `lorawan/sensors` / `eye/sensors`
// ---------------------------------------------------------------------

fn field_prefix(source: &FieldSource) -> (String, AlarmSource) {
    match source {
        // One prefix on both carriers: the P2P arm this had was never taken, and no row holds
        // a `p2p:` id (decision #23, lrw_p2p_unification.cs.md §3).
        FieldSource::Node { node_key } => (
            crate::types::node_sensor_prefix(node_key),
            AlarmSource::Sticker,
        ),
        FieldSource::Beacon { mac } => (format!("eye:{mac}:"), AlarmSource::Eye),
    }
}

/// Reduces one `lorawan/sensors`/`eye/sensors` publish's per-field alarm
/// states. Previous state comes entirely from `active` — there is no
/// separate from/to pair on the wire for field alarms (unlike wired
/// probes), just a snapshot of "what is this field's alarm state now".
pub fn on_field_states(
    active: &dyn ActiveLookup,
    input: &FieldStates,
    ts_iso: &str,
) -> Vec<AlarmAction> {
    let (prefix, source) = field_prefix(&input.source);
    let mut actions = Vec::new();

    for (field, level) in &input.states {
        let label = input
            .labels
            .get(field)
            .cloned()
            .unwrap_or_else(|| field.clone());
        let sensor_id = format!("{prefix}{label}");
        let existing = active.active(&input.hostname, &sensor_id);
        let value = input.values.get(field).copied();

        match level.as_severity() {
            None => {
                if let Some(a) = existing {
                    let ctx = NotifyContext::new(source).with_sensor_name(Some(label.clone()));
                    actions.extend(clear_with_notify(
                        &a,
                        &input.hostname,
                        &sensor_id,
                        ctx,
                        value,
                        ts_iso,
                    ));
                }
            }
            Some(severity) => {
                let thresholds = input.thresholds.get(field);
                let (alarm_type, threshold) = infer_high_low(value, severity, thresholds);
                match existing {
                    None => {
                        let alarm = NewAlarm {
                            hostname: input.hostname.clone(),
                            sensor_id: sensor_id.clone(),
                            sensor_name: Some(label.clone()),
                            alarm_type,
                            severity,
                            from_state: None,
                            to_state: Some(*level),
                            message: None,
                            value,
                            threshold,
                            triggered_at: ts_iso.to_string(),
                            dedup_key: None,
                        };
                        actions.extend(open_with_notify(alarm, NotifyContext::new(source)));
                    }
                    // The side counts as much as the severity (N24, night test): moving the
                    // band across the value turns a critical HIGH into a critical LOW, and the
                    // alarm kept reading HIGH until it cleared.
                    Some(a) if a.severity == severity && a.alarm_type == alarm_type => {}
                    Some(a) => actions.extend(update_severity(
                        &a,
                        &input.hostname,
                        &sensor_id,
                        NotifyContext::new(source).with_sensor_name(Some(label.clone())),
                        SeverityChange {
                            severity,
                            alarm_type,
                            to_state: Some(*level),
                            value,
                            threshold,
                        },
                        ts_iso,
                    )),
                }
            }
        }
    }

    actions
}

// ---------------------------------------------------------------------
// The held grade — what a hysteresis step down is measured from (decision #41)
// ---------------------------------------------------------------------

/// The threshold state a sensor holds now, read off its active alarm: none is `Normal`, an
/// open HIGH/LOW its severity. Any other open alarm (a probe disconnect) is `None`: there is
/// no value grade to hold, so the next grade is strict.
fn held_state(existing: Option<ActiveAlarm>) -> Option<WireState> {
    match existing {
        None => Some(WireState::Normal),
        Some(a) if matches!(a.alarm_type, AlarmType::High | AlarmType::Low) => {
            Some(match a.severity {
                Severity::Critical => WireState::Critical,
                Severity::Warning => WireState::Warning,
            })
        }
        Some(_) => None,
    }
}

/// [`held_state`] of one Node or Beacon field, by the label [`on_field_states`] keys it by.
pub fn held_field_state(
    active: &dyn ActiveLookup,
    hostname: &str,
    source: &FieldSource,
    label: &str,
) -> Option<WireState> {
    let (prefix, _) = field_prefix(source);
    held_state(active.active(hostname, &format!("{prefix}{label}")))
}

/// [`held_state`] of one Hub 1-Wire line.
pub fn held_wired_state(active: &dyn ActiveLookup, hostname: &str, line: i64) -> Option<WireState> {
    held_state(active.active(hostname, &wired_sensor_id(line)))
}

// ---------------------------------------------------------------------
// Native fPort-3 events — `lorawan/sensors` `events[].type == "alarm"`
// ---------------------------------------------------------------------

// Native alarms: fPort 3 on LoRaWAN, frame type 0x03 on P2P. Both key under the one Node
// prefix ([`crate::types::node_sensor_prefix`]).
/// The firmware's no-data watchdog slot (sticker-firmware `app_alarm.c`
/// `APP_ALARM_NO_DATA_SLOT`): one per source, all on this slot.
pub const NO_DATA_SLOT: i64 = 0xFF;

fn native_slot_key(ev: &NativeEvent) -> String {
    match (&ev.source, ev.slot) {
        (Some(source), NO_DATA_SLOT) => format!("{NO_DATA_SLOT}:{source}"),
        _ => ev.slot.to_string(),
    }
}

fn native_sensor_id(ev: &NativeEvent) -> String {
    format!(
        "{NODE_SENSOR_ID_TAG}:{}:native:{}",
        ev.node_key,
        native_slot_key(ev)
    )
}

fn native_dedup_key(ev: &NativeEvent) -> String {
    format!(
        "{}:{}:{}+{}",
        ev.node_key,
        native_slot_key(ev),
        ev.base_time,
        ev.rel_s
    )
}

/// A native alarm is the **Node's** (nodes_phase2.cs.md P2): "Node · Temperature",
/// "Node · Voltage". The sensor's own alarm comes from telemetry.
fn node_alarm_name(quantity: Option<&str>) -> String {
    let Some(q) = quantity.filter(|q| !q.is_empty()) else {
        return "Node".to_string();
    };
    let spaced = q.replace(['-', '_'], " ");
    let mut chars = spaced.chars();
    let titled: String = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => spaced,
    };
    format!("Node \u{b7} {titled}")
}

fn infer_native_side(side: Option<&str>) -> AlarmType {
    match side.map(|s| s.to_ascii_lowercase()) {
        Some(s) if s == "low" => AlarmType::Low,
        // The watchdog's "a sensor stopped delivering": what a disconnect is.
        Some(s) if s == "no_data" => AlarmType::Disconnect,
        // "high", "trigger", unrecognized, or absent -> documented HIGH default.
        _ => AlarmType::High,
    }
}

/// Reduces one native fPort-3 alarm event. Idempotency is by
/// `(node_key, slot, base_time, rel_s)` — exposed as [`NewAlarm::dedup_key`]
/// on the `Open` action so a caller can reject a redelivered raise even
/// once the original alarm has since been cleared.
pub fn on_native_event(active: &dyn ActiveLookup, ev: &NativeEvent) -> Vec<AlarmAction> {
    let sensor_id = native_sensor_id(ev);
    let existing = active.active(&ev.hostname, &sensor_id);
    let name = node_alarm_name(ev.quantity.as_deref());

    match ev.edge {
        Edge::Raise => match existing {
            Some(_) => Vec::new(),
            None => {
                let alarm = NewAlarm {
                    hostname: ev.hostname.clone(),
                    sensor_id: sensor_id.clone(),
                    sensor_name: Some(name),
                    alarm_type: infer_native_side(ev.side.as_deref()),
                    // F23 (nodes_phase2.cs.md P2): every fPort-3 message is an alarm on the
                    // Node, and it is critical. The wire carries no level of its own.
                    severity: Severity::Critical,
                    from_state: None,
                    to_state: None,
                    message: None,
                    value: ev.value,
                    threshold: None,
                    triggered_at: ev.ts_iso.clone(),
                    dedup_key: Some(native_dedup_key(ev)),
                };
                open_with_notify(alarm, NotifyContext::new(AlarmSource::Sticker))
            }
        },
        Edge::Clear => match existing {
            None => Vec::new(),
            Some(a) => {
                let ctx = NotifyContext::new(AlarmSource::Sticker).with_sensor_name(Some(name));
                clear_with_notify(&a, &ev.hostname, &sensor_id, ctx, ev.value, &ev.ts_iso)
            }
        },
    }
}

// ---------------------------------------------------------------------
// Acknowledge-reason validation (rest.md `POST /alarms/{id}/acknowledge`,
// UI-fixes #7 ack-requires-reason).
// ---------------------------------------------------------------------

/// Why an operator-supplied text — an acknowledgement reason or an alarm
/// note — was rejected. A stable slug rather than a sentence, so the portal
/// can translate it (`alarms.errors.<slug>`); the `Display` text is what
/// reaches the log. Same typed-over-stringly rationale as
/// `auth::AuthError::slug`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TextProblem {
    #[error("reason must not be blank")]
    ReasonBlank,
    #[error("reason must be at most 2000 characters")]
    ReasonTooLong,
    #[error("note content must not be blank")]
    NoteBlank,
}

impl TextProblem {
    /// `alarms.errors.<slug>` catalogue key suffix.
    pub fn slug(&self) -> &'static str {
        match self {
            Self::ReasonBlank => "reason_blank",
            Self::ReasonTooLong => "reason_too_long",
            Self::NoteBlank => "note_blank",
        }
    }
}

/// `1..=2000` non-whitespace characters, matching rest.md's
/// `{reason:str (1..2000)}` plus the ack-requires-reason UI fix (a
/// whitespace-only reason must not pass just because it's non-empty).
pub fn ack_validation(reason: &str) -> Result<(), TextProblem> {
    if reason.trim().is_empty() {
        return Err(TextProblem::ReasonBlank);
    }
    if reason.chars().count() > 2000 {
        return Err(TextProblem::ReasonTooLong);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    // -------------------------------------------------------------
    // Test double: a simple in-memory active-alarm table, also used to
    // drive the property test by actually applying returned actions.
    // -------------------------------------------------------------

    #[derive(Default)]
    struct FakeStore {
        rows: RefCell<HashMap<(String, String), ActiveAlarm>>,
        next_id: RefCell<u64>,
    }

    impl FakeStore {
        fn insert(&self, hostname: &str, sensor_id: &str, alarm: ActiveAlarm) {
            self.rows
                .borrow_mut()
                .insert((hostname.to_string(), sensor_id.to_string()), alarm);
        }

        /// Applies a reducer's actions as a real store would, and asserts
        /// the anti-spam invariant along the way: an `Open` must never
        /// land on a key that still holds a *different* active row —
        /// i.e. any pre-existing row for that key must have been `Clear`ed
        /// earlier in this same action batch.
        fn apply(&self, hostname: &str, sensor_id: &str, actions: &[AlarmAction]) {
            let key = (hostname.to_string(), sensor_id.to_string());
            let mut cleared_this_batch = false;
            for action in actions {
                match action {
                    AlarmAction::Open(alarm) => {
                        let already_active = self.rows.borrow().contains_key(&key);
                        assert!(
                            !already_active || cleared_this_batch,
                            "Open applied to {sensor_id} while a different active row was still present \
                             and no Clear preceded it in this batch — would create a second active row"
                        );
                        let mut next_id = self.next_id.borrow_mut();
                        *next_id += 1;
                        self.rows.borrow_mut().insert(
                            key.clone(),
                            ActiveAlarm {
                                id: format!("id-{next_id}"),
                                severity: alarm.severity,
                                max_severity: alarm.severity,
                                alarm_type: alarm.alarm_type,
                                to_state: alarm.to_state,
                                triggered_at_unix: None,
                            },
                        );
                        cleared_this_batch = false;
                    }
                    AlarmAction::UpdateSeverity {
                        severity,
                        alarm_type,
                        to_state,
                        ..
                    } => {
                        if let Some(row) = self.rows.borrow_mut().get_mut(&key) {
                            row.severity = *severity;
                            row.max_severity = row.max_severity.max(*severity);
                            row.alarm_type = *alarm_type;
                            row.to_state = *to_state;
                        }
                    }
                    AlarmAction::UpdateStateInPlace { to_state, .. } => {
                        if let Some(row) = self.rows.borrow_mut().get_mut(&key) {
                            row.to_state = Some(*to_state);
                        }
                    }
                    AlarmAction::Clear { .. } => {
                        self.rows.borrow_mut().remove(&key);
                        cleared_this_batch = true;
                    }
                    AlarmAction::Notify(_) => {}
                }
            }
        }
    }

    impl ActiveLookup for FakeStore {
        fn active(&self, hostname: &str, sensor_id: &str) -> Option<ActiveAlarm> {
            self.rows
                .borrow()
                .get(&(hostname.to_string(), sensor_id.to_string()))
                .cloned()
        }
    }

    fn wired(hostname: &str, line: i64, from: WireState, to: WireState) -> WiredTransition {
        WiredTransition {
            hostname: hostname.to_string(),
            line,
            name: Some("Fridge".to_string()),
            location: None,
            from_state: from,
            to_state: to,
            temperature: Some(5.0),
            thresholds: Some(ThresholdBounds {
                critical_low: Some(-5.0),
                warning_low: Some(0.0),
                warning_high: Some(8.0),
                critical_high: Some(12.0),
            }),
            alarm_type_hint: None,
            ts_iso: "2026-08-18T00:00:00Z".to_string(),
        }
    }

    fn active_alarm(
        id: &str,
        severity: Severity,
        alarm_type: AlarmType,
        to_state: Option<WireState>,
    ) -> ActiveAlarm {
        ActiveAlarm {
            id: id.to_string(),
            severity,
            max_severity: severity,
            alarm_type,
            to_state,
            triggered_at_unix: None,
        }
    }

    // ---------------- wired: snapshot reconcile ----------------

    /// The alarm opened at this instant; snapshots are placed around it.
    const OPENED: i64 = 1_758_800_000;

    fn snapshot(state: WireState, window_start_unix: i64) -> WiredSnapshot {
        WiredSnapshot {
            hostname: "h1".to_string(),
            line: 2,
            name: Some("Fridge".to_string()),
            location: Some("Lab".to_string()),
            state,
            sample_count: 30,
            temperature: Some(5.0),
            window_start_unix,
            window_start_iso: "2026-09-25T12:00:00Z".to_string(),
            now_iso: "2026-09-25T12:07:30Z".to_string(),
        }
    }

    fn store_with(alarm_type: AlarmType, to_state: WireState) -> FakeStore {
        let store = FakeStore::default();
        let mut a = active_alarm("a1", Severity::Warning, alarm_type, Some(to_state));
        a.triggered_at_unix = Some(OPENED);
        store.insert("h1", "line-2", a);
        store
    }

    /// decisions.md #28b (amends #27): the row closes when the portal closes
    /// it, and the window start, the last moment the probe is known NORMAL,
    /// goes into the message.
    #[test]
    fn wired_snapshot_normal_after_the_alarm_clears_it_with_a_reason() {
        let store = store_with(AlarmType::High, WireState::Warning);
        let actions = on_wired_snapshot(&store, &snapshot(WireState::Normal, OPENED + 60));
        assert_eq!(actions.len(), 2);
        let expected =
            format!("{WIRED_RECONCILE_MESSAGE} Last NORMAL window started 2026-09-25T12:00:00Z.");
        match &actions[0] {
            AlarmAction::Clear {
                id,
                cleared_at_iso,
                message,
            } => {
                assert_eq!(id, "a1");
                assert_eq!(cleared_at_iso, "2026-09-25T12:07:30Z", "the portal's time");
                assert_eq!(message.as_deref(), Some(expected.as_str()));
            }
            other => panic!("expected Clear, got {other:?}"),
        }
        match &actions[1] {
            AlarmAction::Notify(NotifyKind::Cleared(p)) => {
                assert_eq!(p.sensor_id, "line-2");
                assert_eq!(p.line, Some(2));
                assert_eq!(p.message.as_deref(), Some(expected.as_str()));
            }
            other => panic!("expected Notify(Cleared), got {other:?}"),
        }
        assert_eq!(
            wired_reconcile_last_normal(&expected),
            Some("2026-09-25T12:00:00Z")
        );
        assert!(is_wired_reconcile(&expected));
        assert!(
            is_wired_reconcile(WIRED_RECONCILE_MESSAGE),
            "a row from before #28b"
        );
        assert_eq!(wired_reconcile_last_normal(WIRED_RECONCILE_MESSAGE), None);
    }

    #[test]
    fn wired_snapshot_clears_a_low_alarm_too() {
        let store = store_with(AlarmType::Low, WireState::Critical);
        let actions = on_wired_snapshot(&store, &snapshot(WireState::Normal, OPENED + 60));
        assert!(matches!(&actions[0], AlarmAction::Clear { id, .. } if id == "a1"));
    }

    #[test]
    fn wired_snapshot_not_after_the_alarm_is_ignored() {
        let store = store_with(AlarmType::High, WireState::Warning);
        for start in [OPENED - 60, OPENED] {
            assert!(on_wired_snapshot(&store, &snapshot(WireState::Normal, start)).is_empty());
        }
    }

    #[test]
    fn wired_snapshot_without_a_parseable_triggered_at_is_ignored() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "line-2",
            active_alarm("a1", Severity::Warning, AlarmType::High, None),
        );
        assert!(on_wired_snapshot(&store, &snapshot(WireState::Normal, OPENED + 60)).is_empty());
    }

    #[test]
    fn wired_snapshot_never_clears_a_disconnect_row() {
        let store = store_with(AlarmType::Disconnect, WireState::Disconnected);
        assert!(on_wired_snapshot(&store, &snapshot(WireState::Normal, OPENED + 60)).is_empty());
    }

    #[test]
    fn wired_snapshot_that_is_not_normal_is_ignored() {
        let store = store_with(AlarmType::High, WireState::Warning);
        for state in [
            WireState::Warning,
            WireState::Critical,
            WireState::Disconnected,
            WireState::Reconnecting,
        ] {
            assert!(on_wired_snapshot(&store, &snapshot(state, OPENED + 60)).is_empty());
        }
    }

    #[test]
    fn wired_snapshot_with_no_samples_is_ignored() {
        let store = store_with(AlarmType::High, WireState::Warning);
        let mut s = snapshot(WireState::Normal, OPENED + 60);
        s.sample_count = 0;
        assert!(on_wired_snapshot(&store, &s).is_empty());
    }

    #[test]
    fn wired_snapshot_without_an_active_row_is_nothing() {
        let store = FakeStore::default();
        assert!(on_wired_snapshot(&store, &snapshot(WireState::Normal, OPENED + 60)).is_empty());
    }

    // ---------------- wired: no active row ----------------

    #[test]
    fn wired_no_active_normal_is_nothing() {
        let store = FakeStore::default();
        let t = wired("h1", 0, WireState::Normal, WireState::Normal);
        assert!(on_wired_transition(&store, &t).is_empty());
    }

    #[test]
    fn wired_no_active_neverconnected_is_nothing() {
        let store = FakeStore::default();
        let t = wired(
            "h1",
            0,
            WireState::NeverConnected,
            WireState::NeverConnected,
        );
        assert!(on_wired_transition(&store, &t).is_empty());
    }

    #[test]
    fn wired_no_active_warning_opens_high_when_value_at_high_bound() {
        let store = FakeStore::default();
        let mut t = wired("h1", 0, WireState::Normal, WireState::Warning);
        t.temperature = Some(9.0); // above warning_high=8.0
        let actions = on_wired_transition(&store, &t);
        assert_eq!(actions.len(), 2);
        match &actions[0] {
            AlarmAction::Open(a) => {
                assert_eq!(a.alarm_type, AlarmType::High);
                assert_eq!(a.severity, Severity::Warning);
                assert_eq!(a.sensor_id, "line-0");
                assert_eq!(a.threshold, Some(8.0));
            }
            other => panic!("expected Open, got {other:?}"),
        }
        assert!(matches!(
            actions[1],
            AlarmAction::Notify(NotifyKind::Activated(_))
        ));
    }

    #[test]
    fn wired_no_active_warning_opens_low_when_value_at_low_bound() {
        let store = FakeStore::default();
        let mut t = wired("h1", 0, WireState::Normal, WireState::Warning);
        t.temperature = Some(-1.0); // below warning_low=0.0
        let actions = on_wired_transition(&store, &t);
        match &actions[0] {
            AlarmAction::Open(a) => assert_eq!(a.alarm_type, AlarmType::Low),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn wired_unknown_thresholds_default_high() {
        let store = FakeStore::default();
        let mut t = wired("h1", 0, WireState::Normal, WireState::Warning);
        t.thresholds = None;
        let actions = on_wired_transition(&store, &t);
        match &actions[0] {
            AlarmAction::Open(a) => {
                assert_eq!(a.alarm_type, AlarmType::High);
                assert_eq!(a.threshold, None);
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn wired_no_active_critical_opens_critical_severity() {
        let store = FakeStore::default();
        let mut t = wired("h1", 0, WireState::Warning, WireState::Critical);
        t.temperature = Some(20.0); // above critical_high=12.0
        let actions = on_wired_transition(&store, &t);
        match &actions[0] {
            AlarmAction::Open(a) => {
                assert_eq!(a.severity, Severity::Critical);
                assert_eq!(a.alarm_type, AlarmType::High);
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn wired_no_active_disconnected_opens_disconnect() {
        let store = FakeStore::default();
        let t = wired("h1", 0, WireState::Normal, WireState::Disconnected);
        let actions = on_wired_transition(&store, &t);
        match &actions[0] {
            AlarmAction::Open(a) => {
                assert_eq!(a.alarm_type, AlarmType::Disconnect);
                assert_eq!(a.severity, Severity::Critical);
            }
            other => panic!("expected Open, got {other:?}"),
        }
        assert!(matches!(
            actions[1],
            AlarmAction::Notify(NotifyKind::Activated(_))
        ));
    }

    // ---------------- wired: active threshold row ----------------

    #[test]
    fn wired_warning_to_critical_updates_severity_not_new_row() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "line-0",
            active_alarm("a1", Severity::Warning, AlarmType::High, None),
        );
        let mut t = wired("h1", 0, WireState::Warning, WireState::Critical);
        t.temperature = Some(20.0); // above critical_high=12.0
        let actions = on_wired_transition(&store, &t);
        assert_eq!(
            actions.len(),
            2,
            "UpdateSeverity + the first escalation's Notify"
        );
        match &actions[1] {
            AlarmAction::Notify(NotifyKind::Escalated(p)) => {
                assert_eq!(p.severity, Severity::Critical);
                assert_eq!(p.value, Some(20.0));
                assert_eq!(p.threshold, Some(12.0));
            }
            other => panic!("expected Notify(Escalated), got {other:?}"),
        }
        match &actions[0] {
            AlarmAction::UpdateSeverity {
                id,
                severity,
                alarm_type,
                to_state,
                value,
                threshold,
                ..
            } => {
                assert_eq!(id, "a1");
                assert_eq!(*severity, Severity::Critical);
                assert_eq!(*alarm_type, AlarmType::High);
                assert_eq!(*to_state, Some(WireState::Critical));
                assert_eq!(*value, Some(20.0));
                // the critical bound it now sits at, not the warning one
                assert_eq!(*threshold, Some(12.0));
            }
            other => panic!("expected UpdateSeverity, got {other:?}"),
        }
    }

    #[test]
    fn wired_critical_to_warning_updates_severity_down() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "line-0",
            active_alarm("a1", Severity::Critical, AlarmType::High, None),
        );
        let mut t = wired("h1", 0, WireState::Critical, WireState::Warning);
        t.temperature = Some(-1.0); // below warning_low=0.0: HIGH row re-infers as LOW
        let actions = on_wired_transition(&store, &t);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            AlarmAction::UpdateSeverity {
                severity,
                alarm_type,
                to_state,
                threshold,
                ..
            } => {
                assert_eq!(*severity, Severity::Warning);
                assert_eq!(*alarm_type, AlarmType::Low);
                assert_eq!(*to_state, Some(WireState::Warning));
                assert_eq!(*threshold, Some(0.0));
            }
            other => panic!("expected UpdateSeverity, got {other:?}"),
        }
    }

    #[test]
    fn wired_duplicate_same_severity_is_nothing() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "line-0",
            active_alarm("a1", Severity::Warning, AlarmType::High, None),
        );
        let t = wired("h1", 0, WireState::Warning, WireState::Warning);
        assert!(on_wired_transition(&store, &t).is_empty());
    }

    #[test]
    fn wired_any_active_to_normal_clears() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "line-0",
            active_alarm("a1", Severity::Critical, AlarmType::High, None),
        );
        let t = wired("h1", 0, WireState::Critical, WireState::Normal);
        let actions = on_wired_transition(&store, &t);
        assert_eq!(actions.len(), 2);
        match &actions[0] {
            AlarmAction::Clear { id, .. } => assert_eq!(id, "a1"),
            other => panic!("expected Clear, got {other:?}"),
        }
        assert!(matches!(
            actions[1],
            AlarmAction::Notify(NotifyKind::Cleared(_))
        ));
    }

    // ---------------- wired: DISCONNECTED <-> RECONNECTING ----------------

    #[test]
    fn wired_disconnected_active_to_reconnecting_updates_in_place() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "line-0",
            active_alarm(
                "a1",
                Severity::Critical,
                AlarmType::Disconnect,
                Some(WireState::Disconnected),
            ),
        );
        let t = wired("h1", 0, WireState::Disconnected, WireState::Reconnecting);
        let actions = on_wired_transition(&store, &t);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            AlarmAction::UpdateStateInPlace { id, to_state, .. } => {
                assert_eq!(id, "a1");
                assert_eq!(*to_state, WireState::Reconnecting);
            }
            other => panic!("expected UpdateStateInPlace, got {other:?}"),
        }
    }

    #[test]
    fn wired_disconnected_duplicate_same_to_state_is_nothing() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "line-0",
            active_alarm(
                "a1",
                Severity::Critical,
                AlarmType::Disconnect,
                Some(WireState::Disconnected),
            ),
        );
        let t = wired("h1", 0, WireState::Disconnected, WireState::Disconnected);
        assert!(on_wired_transition(&store, &t).is_empty());
    }

    #[test]
    fn wired_disconnect_active_to_normal_clears() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "line-0",
            active_alarm(
                "a1",
                Severity::Critical,
                AlarmType::Disconnect,
                Some(WireState::Reconnecting),
            ),
        );
        let t = wired("h1", 0, WireState::Reconnecting, WireState::Normal);
        let actions = on_wired_transition(&store, &t);
        assert!(matches!(actions[0], AlarmAction::Clear { .. }));
    }

    // ---------------- wired: kind supersede ----------------

    #[test]
    fn wired_threshold_active_to_disconnected_clears_then_opens_disconnect() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "line-0",
            active_alarm("a1", Severity::Warning, AlarmType::High, None),
        );
        let t = wired("h1", 0, WireState::Warning, WireState::Disconnected);
        let actions = on_wired_transition(&store, &t);
        assert_eq!(actions.len(), 4); // Clear, Notify(Cleared), Open, Notify(Activated)
        assert!(matches!(&actions[0], AlarmAction::Clear { id, .. } if id == "a1"));
        assert!(matches!(
            actions[1],
            AlarmAction::Notify(NotifyKind::Cleared(_))
        ));
        match &actions[2] {
            AlarmAction::Open(a) => assert_eq!(a.alarm_type, AlarmType::Disconnect),
            other => panic!("expected Open, got {other:?}"),
        }
        assert!(matches!(
            actions[3],
            AlarmAction::Notify(NotifyKind::Activated(_))
        ));
    }

    #[test]
    fn wired_disconnect_active_to_warning_clears_then_opens_threshold() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "line-0",
            active_alarm(
                "a1",
                Severity::Critical,
                AlarmType::Disconnect,
                Some(WireState::Reconnecting),
            ),
        );
        let mut t = wired("h1", 0, WireState::Reconnecting, WireState::Warning);
        t.temperature = Some(9.0);
        let actions = on_wired_transition(&store, &t);
        assert!(matches!(&actions[0], AlarmAction::Clear { id, .. } if id == "a1"));
        match &actions[2] {
            AlarmAction::Open(a) => {
                assert_eq!(a.alarm_type, AlarmType::High);
                assert_eq!(a.severity, Severity::Warning);
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    // ---------------- wired: alarm_type_hint ----------------

    #[test]
    fn wired_hint_power_opens_power_disconnect() {
        let store = FakeStore::default();
        let mut t = wired("h1", -1, WireState::Normal, WireState::Critical);
        t.alarm_type_hint = Some(ConnectivityKind::Power);
        let actions = on_wired_transition(&store, &t);
        match &actions[0] {
            AlarmAction::Open(a) => {
                assert_eq!(a.alarm_type, AlarmType::PowerDisconnect);
                assert_eq!(a.severity, Severity::Critical);
                assert_eq!(a.sensor_id, "sys:power");
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn wired_hint_wifi_clears_on_normal() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "sys:wifi",
            active_alarm("a1", Severity::Warning, AlarmType::WifiDisconnect, None),
        );
        let mut t = wired("h1", -1, WireState::Warning, WireState::Normal);
        t.alarm_type_hint = Some(ConnectivityKind::Wifi);
        let actions = on_wired_transition(&store, &t);
        assert!(matches!(&actions[0], AlarmAction::Clear{ id, .. } if id == "a1"));
    }

    // ---------------- connectivity ----------------

    /// decisions.md #30a: Hub offline is a connectivity alarm `sys:online`,
    /// opened on offline and cleared on return, each with its notification.
    #[test]
    fn hub_offline_opens_sys_online_and_its_return_clears_it() {
        let store = FakeStore::default();
        let snap = |online| ConnectivitySnapshot {
            hostname: "h1".to_string(),
            dc_connected: None,
            wifi_connected: None,
            ethernet_connected: None,
            has_internet: None,
            concentrator_present: None,
            online: Some(online),
            northbridge_reachable: None,
            chirpstack_running: None,
            northbridge_update_ok: None,
            southbridge_update_ok: None,
        };
        let down = on_connectivity(&store, &snap(false), "2026-09-26T10:00:00Z");
        match &down[..] {
            [AlarmAction::Open(a), AlarmAction::Notify(NotifyKind::Activated(p))] => {
                assert_eq!(a.sensor_id, "sys:online");
                assert_eq!(a.alarm_type, AlarmType::HubOffline);
                assert_eq!(a.severity, Severity::Critical);
                assert_eq!(p.source, AlarmSource::System);
            }
            other => panic!("expected Open + Activated, got {other:?}"),
        }
        store.apply("h1", "sys:online", &down);
        assert!(
            on_connectivity(&store, &snap(false), "later").is_empty(),
            "one-shot while open"
        );

        let up = on_connectivity(&store, &snap(true), "2026-09-26T10:05:00Z");
        assert!(matches!(
            &up[..],
            [
                AlarmAction::Clear { .. },
                AlarmAction::Notify(NotifyKind::Cleared(_))
            ]
        ));
    }

    #[test]
    fn connectivity_power_down_opens_critical_when_no_active() {
        let store = FakeStore::default();
        let snap = ConnectivitySnapshot {
            hostname: "h1".into(),
            dc_connected: Some(false),
            wifi_connected: None,
            ethernet_connected: None,
            has_internet: None,
            concentrator_present: None,
            online: None,
            northbridge_reachable: None,
            chirpstack_running: None,
            northbridge_update_ok: None,
            southbridge_update_ok: None,
        };
        let actions = on_connectivity(&store, &snap, "ts");
        assert_eq!(actions.len(), 2);
        match &actions[0] {
            AlarmAction::Open(a) => {
                assert_eq!(a.alarm_type, AlarmType::PowerDisconnect);
                assert_eq!(a.severity, Severity::Critical);
                assert_eq!(a.sensor_id, "sys:power");
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn connectivity_power_down_while_active_is_nothing() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "sys:power",
            active_alarm("a1", Severity::Critical, AlarmType::PowerDisconnect, None),
        );
        let snap = ConnectivitySnapshot {
            hostname: "h1".into(),
            dc_connected: Some(false),
            wifi_connected: None,
            ethernet_connected: None,
            has_internet: None,
            concentrator_present: None,
            online: None,
            northbridge_reachable: None,
            chirpstack_running: None,
            northbridge_update_ok: None,
            southbridge_update_ok: None,
        };
        assert!(on_connectivity(&store, &snap, "ts").is_empty());
    }

    #[test]
    fn connectivity_power_up_while_active_clears() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "sys:power",
            active_alarm("a1", Severity::Critical, AlarmType::PowerDisconnect, None),
        );
        let snap = ConnectivitySnapshot {
            hostname: "h1".into(),
            dc_connected: Some(true),
            wifi_connected: None,
            ethernet_connected: None,
            has_internet: None,
            concentrator_present: None,
            online: None,
            northbridge_reachable: None,
            chirpstack_running: None,
            northbridge_update_ok: None,
            southbridge_update_ok: None,
        };
        let actions = on_connectivity(&store, &snap, "ts");
        assert!(matches!(&actions[0], AlarmAction::Clear{ id, .. } if id == "a1"));
    }

    #[test]
    fn connectivity_power_up_without_active_is_nothing() {
        let store = FakeStore::default();
        let snap = ConnectivitySnapshot {
            hostname: "h1".into(),
            dc_connected: Some(true),
            wifi_connected: None,
            ethernet_connected: None,
            has_internet: None,
            concentrator_present: None,
            online: None,
            northbridge_reachable: None,
            chirpstack_running: None,
            northbridge_update_ok: None,
            southbridge_update_ok: None,
        };
        assert!(on_connectivity(&store, &snap, "ts").is_empty());
    }

    #[test]
    fn connectivity_unknown_leaf_is_nothing() {
        let store = FakeStore::default();
        let snap = ConnectivitySnapshot {
            hostname: "h1".into(),
            dc_connected: None,
            wifi_connected: None,
            ethernet_connected: None,
            has_internet: None,
            concentrator_present: None,
            online: None,
            northbridge_reachable: None,
            chirpstack_running: None,
            northbridge_update_ok: None,
            southbridge_update_ok: None,
        };
        assert!(on_connectivity(&store, &snap, "ts").is_empty());
    }

    #[test]
    fn connectivity_wifi_down_opens_warning_one_shot() {
        let store = FakeStore::default();
        let snap = ConnectivitySnapshot {
            hostname: "h1".into(),
            dc_connected: None,
            wifi_connected: Some(false),
            ethernet_connected: None,
            has_internet: None,
            concentrator_present: None,
            online: None,
            northbridge_reachable: None,
            chirpstack_running: None,
            northbridge_update_ok: None,
            southbridge_update_ok: None,
        };
        let actions = on_connectivity(&store, &snap, "ts");
        match &actions[0] {
            AlarmAction::Open(a) => {
                assert_eq!(a.alarm_type, AlarmType::WifiDisconnect);
                assert_eq!(a.severity, Severity::Warning);
            }
            other => panic!("expected Open, got {other:?}"),
        }
        // Re-raise while active must not happen.
        store.insert(
            "h1",
            "sys:wifi",
            active_alarm("a1", Severity::Warning, AlarmType::WifiDisconnect, None),
        );
        assert!(on_connectivity(&store, &snap, "ts").is_empty());
    }

    #[test]
    fn connectivity_ethernet_up_clears() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "sys:ethernet",
            active_alarm("a1", Severity::Warning, AlarmType::EthernetDisconnect, None),
        );
        let snap = ConnectivitySnapshot {
            hostname: "h1".into(),
            dc_connected: None,
            wifi_connected: None,
            ethernet_connected: Some(true),
            has_internet: None,
            concentrator_present: None,
            online: None,
            northbridge_reachable: None,
            chirpstack_running: None,
            northbridge_update_ok: None,
            southbridge_update_ok: None,
        };
        let actions = on_connectivity(&store, &snap, "ts");
        assert!(matches!(&actions[0], AlarmAction::Clear{ id, .. } if id == "a1"));
    }

    #[test]
    fn connectivity_northbridge_unreachable_opens_critical_and_clears() {
        let snap = |reachable| ConnectivitySnapshot {
            hostname: "h1".into(),
            dc_connected: None,
            wifi_connected: None,
            ethernet_connected: None,
            has_internet: None,
            concentrator_present: None,
            online: None,
            northbridge_reachable: Some(reachable),
            chirpstack_running: None,
            northbridge_update_ok: None,
            southbridge_update_ok: None,
        };
        let store = FakeStore::default();
        let opened = on_connectivity(&store, &snap(false), "2026-09-26T19:00:00Z");
        match &opened[0] {
            AlarmAction::Open(a) => {
                assert_eq!(a.alarm_type, AlarmType::NorthbridgeUnreachable);
                assert_eq!(a.severity, Severity::Critical);
                assert_eq!(a.sensor_id, "sys:northbridge");
            }
            other => panic!("expected Open, got {other:?}"),
        }
        // Reachable again with nothing active: nothing to clear.
        assert!(on_connectivity(&store, &snap(true), "2026-09-26T19:10:00Z").is_empty());
    }

    #[test]
    fn a_failed_northbridge_update_opens_a_warning_and_clears() {
        let snap = |ok| ConnectivitySnapshot {
            hostname: "h1".into(),
            dc_connected: None,
            wifi_connected: None,
            ethernet_connected: None,
            has_internet: None,
            concentrator_present: None,
            online: None,
            northbridge_reachable: None,
            chirpstack_running: None,
            northbridge_update_ok: Some(ok),
            southbridge_update_ok: None,
        };
        let store = FakeStore::default();
        match &on_connectivity(&store, &snap(false), "2026-10-08T12:00:00Z")[0] {
            AlarmAction::Open(a) => {
                assert_eq!(a.alarm_type, AlarmType::NorthbridgeUpdateFailed);
                assert_eq!(a.severity, Severity::Warning);
                assert_eq!(a.sensor_id, "sys:northbridge-update");
            }
            other => panic!("expected Open, got {other:?}"),
        }
        store.insert(
            "h1",
            "sys:northbridge-update",
            active_alarm(
                "a1",
                Severity::Warning,
                AlarmType::NorthbridgeUpdateFailed,
                None,
            ),
        );
        assert!(
            on_connectivity(&store, &snap(false), "2026-10-08T12:01:00Z").is_empty(),
            "one-shot while active"
        );
        let cleared = on_connectivity(&store, &snap(true), "2026-10-08T12:30:00Z");
        assert!(matches!(&cleared[0], AlarmAction::Clear{ id, .. } if id == "a1"));
    }

    #[test]
    fn a_failed_southbridge_update_opens_a_warning_and_clears() {
        let snap = |ok| ConnectivitySnapshot {
            hostname: "h1".into(),
            dc_connected: None,
            wifi_connected: None,
            ethernet_connected: None,
            has_internet: None,
            concentrator_present: None,
            online: None,
            northbridge_reachable: None,
            chirpstack_running: None,
            northbridge_update_ok: None,
            southbridge_update_ok: Some(ok),
        };
        let store = FakeStore::default();
        match &on_connectivity(&store, &snap(false), "2026-10-09T12:00:00Z")[0] {
            AlarmAction::Open(a) => {
                assert_eq!(a.alarm_type, AlarmType::SouthbridgeUpdateFailed);
                assert_eq!(a.severity, Severity::Warning);
                assert_eq!(a.sensor_id, "sys:southbridge-update");
            }
            other => panic!("expected Open, got {other:?}"),
        }
        store.insert(
            "h1",
            "sys:southbridge-update",
            active_alarm(
                "a1",
                Severity::Warning,
                AlarmType::SouthbridgeUpdateFailed,
                None,
            ),
        );
        assert!(
            on_connectivity(&store, &snap(false), "2026-10-09T12:01:00Z").is_empty(),
            "one-shot while active"
        );
        let cleared = on_connectivity(&store, &snap(true), "2026-10-09T12:30:00Z");
        assert!(matches!(&cleared[0], AlarmAction::Clear{ id, .. } if id == "a1"));
    }

    #[test]
    fn connectivity_chirpstack_down_opens_critical() {
        let snap = ConnectivitySnapshot {
            hostname: "h1".into(),
            dc_connected: None,
            wifi_connected: None,
            ethernet_connected: None,
            has_internet: None,
            concentrator_present: None,
            online: None,
            northbridge_reachable: None,
            chirpstack_running: Some(false),
            northbridge_update_ok: None,
            southbridge_update_ok: None,
        };
        let store = FakeStore::default();
        let opened = on_connectivity(&store, &snap, "2026-09-26T19:00:00Z");
        match &opened[0] {
            AlarmAction::Open(a) => {
                assert_eq!(a.alarm_type, AlarmType::ChirpstackDown);
                assert_eq!(a.sensor_id, "sys:chirpstack");
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn connectivity_concentrator_missing_opens_critical_when_no_active() {
        let store = FakeStore::default();
        let snap = ConnectivitySnapshot {
            hostname: "h1".into(),
            dc_connected: None,
            wifi_connected: None,
            ethernet_connected: None,
            has_internet: None,
            concentrator_present: Some(false),
            online: None,
            northbridge_reachable: None,
            chirpstack_running: None,
            northbridge_update_ok: None,
            southbridge_update_ok: None,
        };
        let actions = on_connectivity(&store, &snap, "ts");
        match &actions[0] {
            AlarmAction::Open(a) => {
                assert_eq!(a.alarm_type, AlarmType::ConcentratorMissing);
                assert_eq!(a.severity, Severity::Critical);
                assert_eq!(a.sensor_id, "sys:concentrator");
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn connectivity_concentrator_missing_while_active_is_nothing() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "sys:concentrator",
            active_alarm(
                "a1",
                Severity::Critical,
                AlarmType::ConcentratorMissing,
                None,
            ),
        );
        let snap = ConnectivitySnapshot {
            hostname: "h1".into(),
            dc_connected: None,
            wifi_connected: None,
            ethernet_connected: None,
            has_internet: None,
            concentrator_present: Some(false),
            online: None,
            northbridge_reachable: None,
            chirpstack_running: None,
            northbridge_update_ok: None,
            southbridge_update_ok: None,
        };
        assert!(on_connectivity(&store, &snap, "ts").is_empty());
    }

    #[test]
    fn connectivity_concentrator_present_clears() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "sys:concentrator",
            active_alarm(
                "a1",
                Severity::Critical,
                AlarmType::ConcentratorMissing,
                None,
            ),
        );
        let snap = ConnectivitySnapshot {
            hostname: "h1".into(),
            dc_connected: None,
            wifi_connected: None,
            ethernet_connected: None,
            has_internet: None,
            concentrator_present: Some(true),
            online: None,
            northbridge_reachable: None,
            chirpstack_running: None,
            northbridge_update_ok: None,
            southbridge_update_ok: None,
        };
        let actions = on_connectivity(&store, &snap, "ts");
        assert!(matches!(&actions[0], AlarmAction::Clear{ id, .. } if id == "a1"));
    }

    #[test]
    fn connectivity_concentrator_none_is_nothing() {
        // `None` = off/p2p mode, unknown, or still inside the boot grace — the
        // engine must take no action, not even clear an existing alarm.
        let store = FakeStore::default();
        store.insert(
            "h1",
            "sys:concentrator",
            active_alarm(
                "a1",
                Severity::Critical,
                AlarmType::ConcentratorMissing,
                None,
            ),
        );
        let snap = ConnectivitySnapshot {
            hostname: "h1".into(),
            dc_connected: None,
            wifi_connected: None,
            ethernet_connected: None,
            has_internet: None,
            concentrator_present: None,
            online: None,
            northbridge_reachable: None,
            chirpstack_running: None,
            northbridge_update_ok: None,
            southbridge_update_ok: None,
        };
        assert!(on_connectivity(&store, &snap, "ts").is_empty());
    }

    // ---------------- field states (node / eye) ----------------

    fn node_field_states(field: &str, level: WireState, value: f64) -> FieldStates {
        let mut states = HashMap::new();
        states.insert(field.to_string(), level);
        let mut labels = HashMap::new();
        labels.insert(field.to_string(), "Temperature".to_string());
        let mut values = HashMap::new();
        values.insert(field.to_string(), value);
        let mut thresholds = HashMap::new();
        thresholds.insert(
            field.to_string(),
            ThresholdBounds {
                critical_low: Some(-10.0),
                warning_low: Some(-5.0),
                warning_high: Some(30.0),
                critical_high: Some(40.0),
            },
        );
        FieldStates {
            hostname: "h1".into(),
            source: FieldSource::Node {
                node_key: "abc123".into(),
            },
            states,
            labels,
            values,
            thresholds,
        }
    }

    #[test]
    fn field_no_active_normal_is_nothing() {
        let store = FakeStore::default();
        let input = node_field_states("temperature", WireState::Normal, 20.0);
        assert!(on_field_states(&store, &input, "ts").is_empty());
    }

    #[test]
    fn field_no_active_warning_opens_high() {
        let store = FakeStore::default();
        let input = node_field_states("temperature", WireState::Warning, 31.0);
        let actions = on_field_states(&store, &input, "ts");
        match &actions[0] {
            AlarmAction::Open(a) => {
                assert_eq!(a.alarm_type, AlarmType::High);
                assert_eq!(a.sensor_id, "lrw:abc123:Temperature");
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn field_no_active_warning_opens_low() {
        let store = FakeStore::default();
        let input = node_field_states("temperature", WireState::Warning, -6.0);
        let actions = on_field_states(&store, &input, "ts");
        match &actions[0] {
            AlarmAction::Open(a) => assert_eq!(a.alarm_type, AlarmType::Low),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    /// decisions.md #28c: the first reach of critical takes the
    /// acknowledgement away, since nobody acknowledged the critical state. A
    /// fall back to warning and a second climb keep it: `max_severity` says
    /// the row was critical when it was acknowledged, or before.
    #[test]
    fn only_the_first_escalation_clears_the_acknowledgement() {
        let clears = |actions: &[AlarmAction]| match &actions[0] {
            AlarmAction::UpdateSeverity {
                clears_acknowledgement,
                ..
            } => *clears_acknowledgement,
            other => panic!("expected UpdateSeverity, got {other:?}"),
        };
        let sensor = "lrw:abc123:Temperature";
        let store = FakeStore::default();
        store.insert(
            "h1",
            sensor,
            active_alarm("a1", Severity::Warning, AlarmType::High, None),
        );
        let critical = node_field_states("temperature", WireState::Critical, 45.0);
        let warning = node_field_states("temperature", WireState::Warning, 31.0);

        let up = on_field_states(&store, &critical, "ts");
        assert!(clears(&up), "the first escalation: {up:?}");
        store.apply("h1", sensor, &up);

        let down = on_field_states(&store, &warning, "ts");
        assert!(!clears(&down), "a fall back to warning");
        store.apply("h1", sensor, &down);

        let again = on_field_states(&store, &critical, "ts");
        assert!(!clears(&again), "a second climb");
    }

    /// decisions.md #26: a row that has already been critical says nothing
    /// when it climbs back, and a fall to warning never notifies.
    #[test]
    fn only_the_first_reach_of_critical_notifies() {
        let store = FakeStore::default();
        let mut been_critical = active_alarm("a1", Severity::Warning, AlarmType::High, None);
        been_critical.max_severity = Severity::Critical;
        store.insert("h1", "lrw:abc123:Temperature", been_critical);
        let input = node_field_states("temperature", WireState::Critical, 45.0);
        let actions = on_field_states(&store, &input, "ts");
        assert_eq!(actions.len(), 1, "UpdateSeverity only: {actions:?}");

        store.insert(
            "h1",
            "lrw:abc123:Temperature",
            active_alarm("a1", Severity::Critical, AlarmType::High, None),
        );
        let input = node_field_states("temperature", WireState::Warning, 35.0);
        let actions = on_field_states(&store, &input, "ts");
        assert!(
            actions.iter().all(|a| !matches!(a, AlarmAction::Notify(_))),
            "a fall back to warning is silent: {actions:?}"
        );
    }

    /// N24 (night test): the band moved across the value, so a critical HIGH is now a critical
    /// LOW. Same severity, other side: the alarm follows, it does not keep reading HIGH.
    #[test]
    fn field_side_flip_at_the_same_severity_updates() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "lrw:abc123:Temperature",
            active_alarm("a1", Severity::Critical, AlarmType::High, None),
        );
        let input = node_field_states("temperature", WireState::Critical, -12.0);
        let actions = on_field_states(&store, &input, "ts");
        match actions.as_slice() {
            [AlarmAction::UpdateSeverity {
                alarm_type,
                threshold,
                ..
            }] => {
                assert_eq!(*alarm_type, AlarmType::Low);
                assert_eq!(*threshold, Some(-10.0));
            }
            other => panic!("expected one UpdateSeverity, got {other:?}"),
        }

        // Same side, same severity: nothing to do.
        let input = node_field_states("temperature", WireState::Critical, 45.0);
        assert!(on_field_states(&store, &input, "ts").is_empty());
    }

    #[test]
    fn field_severity_escalation_updates() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "lrw:abc123:Temperature",
            active_alarm("a1", Severity::Warning, AlarmType::High, None),
        );
        let input = node_field_states("temperature", WireState::Critical, 45.0);
        let actions = on_field_states(&store, &input, "ts");
        assert_eq!(
            actions.len(),
            2,
            "UpdateSeverity + the first escalation's Notify"
        );
        assert!(matches!(
            &actions[1],
            AlarmAction::Notify(NotifyKind::Escalated(p)) if p.sensor_name.as_deref() == Some("Temperature")
        ));
        match &actions[0] {
            AlarmAction::UpdateSeverity {
                severity,
                alarm_type,
                to_state,
                value,
                threshold,
                ..
            } => {
                assert_eq!(*severity, Severity::Critical);
                assert_eq!(*alarm_type, AlarmType::High);
                assert_eq!(*to_state, Some(WireState::Critical));
                assert_eq!(*value, Some(45.0));
                assert_eq!(*threshold, Some(40.0)); // critical_high, not warning_high
            }
            other => panic!("expected UpdateSeverity, got {other:?}"),
        }
    }

    #[test]
    fn field_severity_deescalation_updates() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "lrw:abc123:Temperature",
            active_alarm("a1", Severity::Critical, AlarmType::High, None),
        );
        let input = node_field_states("temperature", WireState::Warning, 31.0);
        let actions = on_field_states(&store, &input, "ts");
        match &actions[0] {
            AlarmAction::UpdateSeverity {
                severity,
                to_state,
                threshold,
                ..
            } => {
                assert_eq!(*severity, Severity::Warning);
                assert_eq!(*to_state, Some(WireState::Warning));
                assert_eq!(*threshold, Some(30.0));
            }
            other => panic!("expected UpdateSeverity, got {other:?}"),
        }
    }

    #[test]
    fn field_duplicate_same_severity_is_nothing() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "lrw:abc123:Temperature",
            active_alarm("a1", Severity::Warning, AlarmType::High, None),
        );
        let input = node_field_states("temperature", WireState::Warning, 31.0);
        assert!(on_field_states(&store, &input, "ts").is_empty());
    }

    #[test]
    fn field_active_to_normal_clears() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "lrw:abc123:Temperature",
            active_alarm("a1", Severity::Warning, AlarmType::High, None),
        );
        let input = node_field_states("temperature", WireState::Normal, 20.0);
        let actions = on_field_states(&store, &input, "ts");
        assert!(matches!(&actions[0], AlarmAction::Clear{ id, .. } if id == "a1"));
    }

    #[test]
    fn field_eye_source_uses_eye_sensor_id_prefix() {
        let store = FakeStore::default();
        let mut input = node_field_states("temperature", WireState::Warning, 31.0);
        input.source = FieldSource::Beacon {
            mac: "AA:BB:CC:DD:EE:FF".into(),
        };
        let actions = on_field_states(&store, &input, "ts");
        match &actions[0] {
            AlarmAction::Open(a) => assert_eq!(a.sensor_id, "eye:AA:BB:CC:DD:EE:FF:Temperature"),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn field_missing_thresholds_default_high() {
        let store = FakeStore::default();
        let mut input = node_field_states("temperature", WireState::Warning, 31.0);
        input.thresholds.clear();
        let actions = on_field_states(&store, &input, "ts");
        match &actions[0] {
            AlarmAction::Open(a) => assert_eq!(a.alarm_type, AlarmType::High),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    // ---------------- native fPort-3 ----------------

    /// nodes_phase2.cs.md P1: critical outranks warning, an unset bound watches nothing.
    #[test]
    fn bounds_classify_critical_before_warning() {
        let b = ThresholdBounds {
            critical_low: Some(5.0),
            warning_low: Some(10.0),
            warning_high: Some(30.0),
            critical_high: Some(35.0),
        };
        assert_eq!(b.classify(20.0), WireState::Normal);
        assert_eq!(b.classify(31.0), WireState::Warning);
        assert_eq!(b.classify(8.0), WireState::Warning);
        assert_eq!(b.classify(36.0), WireState::Critical);
        assert_eq!(b.classify(4.0), WireState::Critical);
        assert_eq!(
            b.classify(35.0),
            WireState::Warning,
            "a bound itself is inside"
        );
        let critical_only = ThresholdBounds {
            critical_low: None,
            warning_low: None,
            warning_high: None,
            critical_high: Some(25.0),
        };
        assert_eq!(critical_only.classify(32.4), WireState::Critical);
        assert_eq!(critical_only.classify(-40.0), WireState::Normal);
    }

    /// nodes_phase2.cs.md P2: an fPort-3 event is a critical alarm on the Node, named after
    /// its quantity; the no-data watchdog keys each source apart.
    #[test]
    fn a_native_event_is_a_critical_node_alarm() {
        let store = FakeStore::default();
        let raise = native(Edge::Raise, Some("high"));
        let actions = on_native_event(&store, &raise);
        match actions.first() {
            Some(AlarmAction::Open(a)) => {
                assert_eq!(a.severity, Severity::Critical);
                assert_eq!(a.sensor_name.as_deref(), Some("Node \u{b7} Temperature"));
                assert_eq!(a.sensor_id, "lrw:abc123:native:2");
            }
            other => panic!("expected an open: {other:?}"),
        }

        let no_data = |source: &str| NativeEvent {
            slot: NO_DATA_SLOT,
            source: Some(source.to_string()),
            side: Some("no_data".to_string()),
            ..native(Edge::Raise, None)
        };
        let ids: Vec<(String, AlarmType, Option<String>)> = ["onboard", "s1"]
            .iter()
            .map(|src| match on_native_event(&store, &no_data(src)).first() {
                Some(AlarmAction::Open(a)) => {
                    (a.sensor_id.clone(), a.alarm_type, a.dedup_key.clone())
                }
                other => panic!("expected an open: {other:?}"),
            })
            .collect();
        assert_eq!(ids[0].0, "lrw:abc123:native:255:onboard");
        assert_eq!(ids[1].0, "lrw:abc123:native:255:s1");
        assert_eq!(
            ids[0].1,
            AlarmType::Disconnect,
            "no data is a sensor gone quiet"
        );
        assert_ne!(ids[0].2, ids[1].2, "two sources, two dedup keys");

        let voltage = NativeEvent {
            slot: 0xFE,
            source: Some("battery".into()),
            quantity: Some("voltage".into()),
            side: Some("low".into()),
            ..native(Edge::Raise, None)
        };
        match on_native_event(&store, &voltage).first() {
            Some(AlarmAction::Open(a)) => {
                assert_eq!(a.sensor_name.as_deref(), Some("Node \u{b7} Voltage"));
                assert_eq!(a.alarm_type, AlarmType::Low);
            }
            other => panic!("expected an open: {other:?}"),
        }
    }

    fn native(edge: Edge, side: Option<&str>) -> NativeEvent {
        NativeEvent {
            hostname: "h1".into(),
            node_key: "abc123".into(),
            source: None,
            slot: 2,
            quantity: Some("temperature".into()),
            edge,
            side: side.map(|s| s.to_string()),
            value: Some(42.0),
            base_time: 1000,
            rel_s: 30,
            ts_iso: "ts".into(),
        }
    }

    #[test]
    fn native_raise_no_active_opens_with_dedup_key() {
        let store = FakeStore::default();
        let ev = native(Edge::Raise, Some("high"));
        let actions = on_native_event(&store, &ev);
        match &actions[0] {
            AlarmAction::Open(a) => {
                assert_eq!(a.sensor_id, "lrw:abc123:native:2");
                assert_eq!(a.alarm_type, AlarmType::High);
                assert_eq!(a.dedup_key.as_deref(), Some("abc123:2:1000+30"));
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn native_raise_low_side() {
        let store = FakeStore::default();
        let ev = native(Edge::Raise, Some("LOW"));
        let actions = on_native_event(&store, &ev);
        match &actions[0] {
            AlarmAction::Open(a) => assert_eq!(a.alarm_type, AlarmType::Low),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn native_raise_missing_side_defaults_high() {
        let store = FakeStore::default();
        let ev = native(Edge::Raise, None);
        let actions = on_native_event(&store, &ev);
        match &actions[0] {
            AlarmAction::Open(a) => assert_eq!(a.alarm_type, AlarmType::High),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn native_raise_duplicate_while_active_is_nothing() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "lrw:abc123:native:2",
            active_alarm("a1", Severity::Warning, AlarmType::High, None),
        );
        let ev = native(Edge::Raise, Some("high"));
        assert!(on_native_event(&store, &ev).is_empty());
    }

    #[test]
    fn native_clear_active_clears() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "lrw:abc123:native:2",
            active_alarm("a1", Severity::Warning, AlarmType::High, None),
        );
        let ev = native(Edge::Clear, Some("high"));
        let actions = on_native_event(&store, &ev);
        assert!(matches!(&actions[0], AlarmAction::Clear{ id, .. } if id == "a1"));
    }

    #[test]
    fn native_clear_without_active_is_nothing() {
        let store = FakeStore::default();
        let ev = native(Edge::Clear, Some("high"));
        assert!(on_native_event(&store, &ev).is_empty());
    }

    // ---------------- ack_validation ----------------

    #[test]
    fn ack_empty_is_err() {
        assert!(ack_validation("").is_err());
    }

    #[test]
    fn ack_whitespace_only_is_err() {
        assert!(ack_validation("   \t\n").is_err());
    }

    #[test]
    fn ack_valid_reason_is_ok() {
        assert!(ack_validation("Confirmed with on-site technician").is_ok());
    }

    #[test]
    fn ack_exactly_2000_chars_is_ok() {
        let reason = "a".repeat(2000);
        assert!(ack_validation(&reason).is_ok());
    }

    #[test]
    fn ack_over_2000_chars_is_err() {
        let reason = "a".repeat(2001);
        assert!(ack_validation(&reason).is_err());
    }

    // ---------------- property: never a second active row ----------------

    /// Drives a scripted sequence of wired transitions — including both
    /// "kind supersede" cases — through a real apply loop. `FakeStore::apply`
    /// asserts the anti-spam invariant on every `Open`, so this test fails
    /// loudly if any reducer path ever tries to leave two active rows
    /// behind for the same `(hostname, sensor_id)`.
    #[test]
    fn property_never_two_active_rows_for_same_sensor() {
        let store = FakeStore::default();
        let hostname = "h1";
        let sensor_id = "line-4";

        let script = [
            (WireState::Normal, WireState::Warning),
            (WireState::Warning, WireState::Warning), // duplicate
            (WireState::Warning, WireState::Critical), // escalate
            (WireState::Critical, WireState::Disconnected), // kind supersede
            (WireState::Disconnected, WireState::Reconnecting),
            (WireState::Reconnecting, WireState::Reconnecting), // duplicate
            (WireState::Reconnecting, WireState::Warning),      // kind supersede back
            (WireState::Warning, WireState::Normal),            // clear
            (WireState::Normal, WireState::Disconnected),
            (WireState::Disconnected, WireState::Normal),
        ];

        for (from, to) in script {
            let mut t = wired(hostname, 4, from, to);
            t.temperature = Some(35.0); // comfortably above every high bound
            let actions = on_wired_transition(&store, &t);
            store.apply(hostname, sensor_id, &actions);
        }
    }

    /// Same property, forced through the two specific "kind supersede"
    /// cases with a pre-seeded active row, checked directly against a real
    /// `apply` (rather than only the scripted walk above).
    #[test]
    fn property_kind_supersede_never_leaves_two_active_rows() {
        let store = FakeStore::default();
        store.insert(
            "h1",
            "line-0",
            active_alarm("x1", Severity::Warning, AlarmType::High, None),
        );
        let actions = on_wired_transition(
            &store,
            &wired("h1", 0, WireState::Warning, WireState::Disconnected),
        );
        store.apply("h1", "line-0", &actions); // panics internally if the invariant breaks

        let store2 = FakeStore::default();
        store2.insert(
            "h1",
            "line-0",
            active_alarm(
                "x2",
                Severity::Critical,
                AlarmType::Disconnect,
                Some(WireState::Disconnected),
            ),
        );
        let actions2 = on_wired_transition(
            &store2,
            &wired("h1", 0, WireState::Disconnected, WireState::Warning),
        );
        store2.apply("h1", "line-0", &actions2);
    }
}
