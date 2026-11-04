//! The probe-detail page (MB3) — plan/portal/monitoring.backend.md §6,
//! spec plan/portal/monitoring.md §6.
//!
//! One page showing **one probe** over a chosen range: every field it
//! reports in one chart, count/min/max/avg/median per field, the three
//! classes of event on one timeline, and the two exports. It is a *data*
//! view, not device management — configuration, thresholds and service stay
//! on `/nodes/{key}` and `/beacons/{mac}`, linked from its header.
//!
//! It was a modal over `/monitoring` until 2026-09-15. A wired probe is the
//! only sensor kind with no page of its own, so the same click on the same
//! list opened a modal for one row and left a Node's real page one link
//! further in; pages/probe_detail.html replaced it and took the dialog, its
//! opener and its pushState bookkeeping out of pages/dashboard.html.
//!
//! ## Probe-scoped, not group-scoped
//!
//! The page is addressed by the **member token** (`probe:{line}` /
//! `node:{key}` / `beacon:{MAC}`) and carries no group id: the same probe
//! opened from two groups is the same page, and it shows every field the
//! probe reports even when the group that was clicked holds only one of
//! them (MB1 made exclusivity per field). That is also why a manual event
//! survives its probe moving between groups — the event, like this route,
//! is keyed by the probe.
//!
//! `monitoring::parse_token` is the one parser (MB2 added it; MB3 reuses
//! it rather than growing a second one, which would have to be kept in step
//! with the first).
//!
//! ## Routes
//!
//! | Path | Method | Gate | Audit |
//! |---|---|---|---|
//! | `/monitoring/probe/{token}?range=` | GET | CurrentUser | none |
//! | `/api/v1/chart/eye/{mac}/{field}` | GET | CurrentUser | none |
//! | `/export/probe/{token_csv}?from&to` | GET | CurrentUser | `export` |
//! | `/monitoring/probe/{token}/events` | POST | Manager | `probe_event_create` |
//! | `/monitoring/probe/{token}/events/{id}/delete` | POST | Manager | `probe_event_delete` |
//!
//! Opening the page writes **no** audit row (decision 2026-09-08): it reads
//! exactly what the `/monitoring` page already shows this session, and one
//! row per open would drown the trail the privileged actions need.
//!
//! Every handler answers 4xx or a redirect — never a 500 — for a token that
//! resolves to nothing, because `http.rs`'s `MUTATING_ROUTES` sweep POSTs
//! to these paths as a manager with a deliberately unresolvable
//! `probe:0`.
//!
//! ## Ranges
//!
//! [`PROBE_RANGES`] is the window's own pill set and is resolved **here**
//! into explicit `from`/`to` seconds, which the island then sends as
//! `range=custom&from&to`. `3d` is deliberately not added to
//! `history_routes::resolve_range_parts`: that function backs `/history`'s
//! own pills and every chart endpoint's `?range=`, and widening it would
//! put a pill on pages that do not offer one.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use portal_storage::{
    AlarmListFilter, EyeField, FieldSummary, NewSensorEvent, ProbeIdentity, SensorGroupMember,
};
use serde::{Deserialize, Serialize};

use crate::auth::{CurrentUser, RequireManager};
use crate::http::AppState;
use crate::monitoring::{self, MemberRef};
use crate::render;
use crate::state::{BeaconState, DeviceState, NodeState, PortalState, SensorState};

use super::group_routes::{member_csv_rows, slug};
use super::history_routes::{
    format_rfc3339, parse_iso8601, today_yyyymmdd, ChartPoint, CSV_HEADER, SERIES_COLORS,
};
use super::{audit_action, render as render_page, tr};

/// The window's three range pills (monitoring.md §6.1: 24 h default, 7 d
/// maximum, "delší pohled = odkaz na /history").
const PROBE_RANGES: [&str; 3] = ["24h", "3d", "7d"];

/// How many `alarm_events` rows one page of the window's walk asks for, and
/// how many pages it will walk before giving up and saying so.
///
/// The walk exists because `ORDER BY triggered_at DESC` is **lexicographic**
/// over RFC3339 strings that carry their own UTC offsets, so a single
/// `LIMIT` cannot be trusted to have reached the range's start: a flapping
/// probe would silently truncate its own history.
const ALARM_PAGE_LIMIT: i64 = 500;
const ALARM_MAX_PAGES: usize = 20;

/// How far a lexicographic ordering of offset-carrying timestamps can
/// disagree with a chronological one: the widest legal offsets are `+14:00`
/// and `-12:00`, so two rows whose *parsed* times differ by more than 26 h
/// cannot be swapped by the string comparison. The walk stops only once it
/// is that far past `from`.
const OFFSET_SKEW_S: i64 = 26 * 3600;

/// The longest text a manual event may carry — V12's CHECK, restated here
/// so the operator gets a translated error instead of a constraint
/// violation.
const EVENT_TEXT_MAX: usize = 240;

/// Every `kind` a [`ProbeEventCtx`] can carry — the three classes of
/// monitoring.md §6.3, with the value and liveness classes split into their
/// two edges.
///
/// A closed list, and the source for two things that would otherwise drift:
/// the template's dynamic `monitoring.probe_detail.event_{kind}` lookup and
/// the icon it picks. `every_probe_detail_dynamic_key_exists` enumerates it
/// against both catalogues, which is the only way a dynamic key gets the
/// coverage `every_static_template_key_exists` gives a literal one.
const PROBE_EVENT_KINDS: [&str; 6] = [
    EVENT_VALUE_RAISED,
    EVENT_VALUE_CLEARED,
    EVENT_OFFLINE,
    EVENT_ONLINE,
    EVENT_DISCONNECT,
    EVENT_MANUAL,
];

const EVENT_VALUE_RAISED: &str = "value_raised";
const EVENT_VALUE_CLEARED: &str = "value_cleared";
const EVENT_OFFLINE: &str = "offline";
const EVENT_ONLINE: &str = "online";
/// The firmware's own `DISCONNECT` (and the `POWER_`/`WIFI_`/`ETHERNET_`
/// variants): a wired line whose probe was unplugged, which is a different
/// thing from the portal's `OFFLINE` verdict *and* from a value leaving its
/// bounds. Before this kind existed both rendered as "out of range", which
/// is the one reading that is certainly wrong.
const EVENT_DISCONNECT: &str = "disconnect";
const EVENT_MANUAL: &str = "manual";

/// How one `alarm_events` row reads in the window's timeline.
///
/// The `alarm_type` vocabulary is `portal_domain::types`': `HIGH`/`LOW` (and
/// the native slots) are values, `OFFLINE` is the portal's own liveness
/// verdict, and everything ending in `DISCONNECT` is the firmware saying a
/// physical connection went away.
enum EventClass {
    Offline,
    Disconnect,
    Value,
}

fn classify(alarm_type: &str) -> EventClass {
    if alarm_type.eq_ignore_ascii_case("OFFLINE") {
        EventClass::Offline
    } else if alarm_type.to_ascii_uppercase().ends_with("DISCONNECT") {
        EventClass::Disconnect
    } else {
        EventClass::Value
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        // GETs first: `every_mutating_route_declares_a_role` cuts a
        // registration's span at the *next* `.route(`, so a GET registered
        // before a POST cannot be handed the POST's `post(` and reported as
        // an undeclared mutation.
        .route("/monitoring/probe/{token}", get(probe_detail_page))
        // A wired probe's page (devices.hub.md §3.2, amendment 2026-10-14),
        // registered here beside the window it is built on.
        .route(
            "/hubs/{hub}/sensors/{probe}",
            get(super::hub_routes::hub_sensor_page),
        )
        .route("/api/v1/chart/eye/{mac}/{field}", get(chart_json_eye))
        .route("/export/probe/{token_csv}", get(export_probe))
        .route("/monitoring/probe/{token}/events", post(create_event))
        .route(
            "/monitoring/probe/{token}/events/{id}/delete",
            post(delete_event),
        )
}

// ---------------------------------------------------------------------
// Resolving a token against the live snapshot
// ---------------------------------------------------------------------

/// One probe's field, as the window understands it.
///
/// **The window's own field list**, and the only one: the chart series, the
/// aggregate line and the CSV export all read it, which is what settles the
/// `chartable`-vs-`thresholdable` mismatch monitoring.backend.md §6 WP3.3
/// records between the group export ("Continuous && chartable") and the
/// rendered row ("thresholdable minus battery"). Those two sets are in fact
/// equal for a Node — the only continuous field that is thresholdable and
/// not chartable is `voltage`, which is in the Battery group and excluded
/// by both — and this list is built once so they cannot drift apart the day
/// a registry entry changes.
struct ProbeField {
    /// `None` for a wired probe, which has one unnamed temperature series.
    name: Option<&'static str>,
    label: String,
    unit: String,
    chart_url: String,
}

/// A probe resolved out of the store: who it is, and what it reports.
struct ResolvedProbe {
    token: String,
    kind: &'static str,
    hostname: String,
    device_label: String,
    name: String,
    description: Option<String>,
    fields: Vec<ProbeField>,
    /// The `alarm_events.sensor_id` shape this probe's alarms carry —
    /// `Exact` for a wired line, `Prefix` for a radio device.
    alarm_key: AlarmKey,
    probe: ProbeIdentity,
    /// `/nodes/{key}` for a Node, `/beacons/{mac}` for a Beacon (B5 built
    /// that page). A **wired probe has none** — there is no per-line device
    /// page anywhere in the portal — which is the asymmetry monitoring.md §6
    /// records under the wireframe (clarification 2026-09-10). See
    /// [`ResolvedProbe::history_href`] for the sibling one.
    device_href: Option<String>,
    /// `/history?s=…`, omitted for a Beacon.
    ///
    /// Recorded asymmetry: `history_routes::Selection` has exactly two
    /// variants, `Line` and `NodeField`, and no Beacon one — `/history`
    /// cannot select a Beacon at all today, so linking there would land the
    /// operator on an empty picker. The link is dropped rather than faked.
    history_href: Option<String>,
    /// `false` for a wired probe: it has no per-probe liveness of its own,
    /// only the Hub's `DeviceOffline`, which is a different scope
    /// (monitoring.backend.md §9.2). The window says so rather than showing
    /// an empty operational section that reads as "never offline".
    operational_supported: bool,
    /// The probe's own page: `/hubs/{hub}/sensors/line-{n}` for a wired
    /// line (devices.hub.md §3.2, amendment 2026-10-14), this route's
    /// `/monitoring/probe/{token}` otherwise. The range pills and the manual
    /// event redirects go back here.
    page: String,
}

/// A stored value of one of `probe`'s fields as displayed: a Node field
/// through its registry entry (UX-038: pressure in hPa), anything else as
/// stored.
fn node_display(probe: &ResolvedProbe, field: Option<&str>) -> impl Fn(f64) -> f64 {
    let def = field
        .filter(|_| probe.kind == "node")
        .and_then(fiber_protocol::lorawan_field);
    move |v| def.map_or(v, |f| f.to_display(v))
}

/// How a probe's `alarm_events` rows are addressed.
enum AlarmKey {
    /// A wired probe: `line-{n}` is the whole `sensor_id`.
    Exact(String),
    /// A radio probe: `lrw:{key}:` / `eye:{MAC}:`, shared by every field
    /// label, every `native:{slot}` and MB3's `liveness`.
    Prefix(String),
}

impl ResolvedProbe {
    fn from_sensor(device: &DeviceState, s: &SensorState) -> ResolvedProbe {
        let line = s.line;
        let name = s.name.clone().unwrap_or_else(|| format!("Line {line}"));
        ResolvedProbe {
            token: format!("probe:{line}"),
            kind: "probe",
            hostname: device.hostname.clone(),
            device_label: device_label(device),
            name,
            description: s.location.clone(),
            fields: vec![ProbeField {
                name: None,
                label: tr("monitoring.probe_detail.field_temperature"),
                unit: "°C".to_string(),
                chart_url: format!("/api/v1/chart/{line}"),
            }],
            alarm_key: AlarmKey::Exact(format!("line-{line}")),
            probe: ProbeIdentity::Line(i64::from(line)),
            device_href: None,
            // The token names the probe's Hub (`line:{hub}:{n}`, devices.hub.md
            // amendment 2026-10-15).
            history_href: Some(format!(
                "/trends?s=line:{}:{line}",
                device.hostname.to_lowercase()
            )),
            operational_supported: false,
            page: crate::monitoring::probe_href(&device.hostname, line),
        }
    }

    /// `stored`: the fields the Node has readings for in storage. Right
    /// after a portal start the live state holds no field until the Node's
    /// next uplink, and the window must not call stored history empty
    /// meanwhile (UX-039), so either source puts a field on the page.
    fn from_node(
        device: &DeviceState,
        n: &NodeState,
        stored: &std::collections::HashSet<String>,
    ) -> ResolvedProbe {
        // The same predicate `monitoring::node_row` renders by, minus the
        // group's field selection: the window shows every field of the
        // probe regardless of which group it was opened from. `chartable`
        // is asserted rather than assumed — the chart endpoint refuses a
        // field that is not, so a non-chartable series would be an axis
        // with a permanent fetch error under it.
        let fields = fiber_protocol::lorawan_registry()
            .iter()
            .filter(|f| {
                f.kind == fiber_protocol::LorawanFieldKind::Continuous
                    && f.thresholdable
                    && f.chartable
                    && f.group != fiber_protocol::LorawanFieldGroup::Battery
            })
            .filter(|f| n.fields.contains_key(f.name) || stored.contains(f.name))
            .map(|f| ProbeField {
                name: Some(f.name),
                label: f.label.to_string(),
                unit: f.display_unit().unwrap_or("").to_string(),
                chart_url: format!("/api/v1/chart/node/{}/{}", n.node_key, f.name),
            })
            .collect();
        let name = n.name.clone().unwrap_or_else(|| n.node_key.clone());
        ResolvedProbe {
            token: format!("node:{}", n.node_key),
            kind: "node",
            hostname: device.hostname.clone(),
            device_label: device_label(device),
            name,
            description: n.location.clone(),
            fields,
            alarm_key: AlarmKey::Prefix(portal_domain::node_sensor_prefix(&n.node_key)),
            probe: ProbeIdentity::Node(n.node_key.clone()),
            device_href: Some(format!("/nodes/{}/overview", n.node_key)),
            history_href: Some(format!("/trends?s=node:{}:temperature", n.node_key)),
            operational_supported: true,
            page: format!("/monitoring/probe/node:{}", n.node_key),
        }
    }

    fn from_beacon(device: &DeviceState, b: &BeaconState) -> ResolvedProbe {
        let fields = fiber_protocol::eye_registry()
            .iter()
            .filter(|f| f.thresholdable && f.name != "battery")
            .filter(|f| EyeField::from_field_name(f.name).is_some())
            .map(|f| ProbeField {
                name: Some(f.name),
                label: f.label.to_string(),
                unit: f.unit.unwrap_or("").to_string(),
                chart_url: format!("/api/v1/chart/eye/{}/{}", b.mac, f.name),
            })
            .collect();
        let name = b.name.clone().unwrap_or_else(|| b.mac.clone());
        ResolvedProbe {
            token: format!("beacon:{}", b.mac),
            kind: "beacon",
            hostname: device.hostname.clone(),
            device_label: device_label(device),
            name,
            description: b.location.clone(),
            fields,
            alarm_key: AlarmKey::Prefix(format!("eye:{}:", b.mac)),
            probe: ProbeIdentity::Mac(b.mac.clone()),
            device_href: Some(format!("/beacons/{}/overview", b.mac)),
            history_href: None,
            operational_supported: true,
            page: format!("/monitoring/probe/beacon:{}", b.mac),
        }
    }
}

fn device_label(device: &DeviceState) -> String {
    device
        .system
        .device_label
        .clone()
        .unwrap_or_else(|| device.hostname.clone())
}

/// Finds the probe a token names anywhere in the snapshot.
///
/// Scans every Hub rather than only the first, unlike `member_token`'s own
/// hostname assumption (§14.5): the window is addressed by identity, and a
/// linear pass over a handful of devices is cheaper than deciding which Hub
/// the operator meant.
fn resolve_probe(
    state: &PortalState,
    member_ref: &MemberRef,
    stored: &std::collections::HashSet<String>,
) -> Option<ResolvedProbe> {
    for device in state.devices.values() {
        match member_ref {
            MemberRef::Probe(line) => {
                if let Some(s) = device.sensors.iter().find(|s| i64::from(s.line) == *line) {
                    return Some(ResolvedProbe::from_sensor(device, s));
                }
            }
            MemberRef::Node(key) => {
                if let Some(n) = device.nodes.get(key) {
                    return Some(ResolvedProbe::from_node(device, n, stored));
                }
            }
            MemberRef::Beacon(_) => break,
        }
    }
    if let MemberRef::Beacon(mac) = member_ref {
        // Through `hub_of_beacon`, not the first Hub that has the MAC: in a
        // cluster another Hub may overhear the tag as a stranger, and its
        // copy sorting first used to answer "unknown probe" for a Beacon
        // registered elsewhere. A merely discovered tag is not a probe: it
        // is a candidate on `/beacons`, with no stored history to summarise.
        // Same rule `monitoring::resolve_row` applies.
        let (device, b) = state.hub_of_beacon(mac)?;
        if b.discovered == Some(true) {
            return None;
        }
        return Some(ResolvedProbe::from_beacon(device, b));
    }
    None
}

/// Token -> probe, or the status a handler should answer with.
async fn lookup(state: &AppState, token: &str) -> Result<ResolvedProbe, StatusCode> {
    lookup_in(state, token, None).await
}

/// [`lookup`] with the Hub named: a wired line exists on every Hub, so
/// `probe:{line}` alone resolves to the first one; the per-probe page on
/// the Hub (and its event forms, which post `hub`) name theirs.
async fn lookup_in(
    state: &AppState,
    token: &str,
    hub: Option<&str>,
) -> Result<ResolvedProbe, StatusCode> {
    let member_ref = monitoring::parse_token(token).ok_or(StatusCode::BAD_REQUEST)?;
    let snapshot = state.store.snapshot();
    if let (MemberRef::Probe(line), Some(hub)) = (&member_ref, hub.filter(|h| !h.is_empty())) {
        let device = snapshot
            .devices
            .values()
            .find(|d| d.hostname.trim().eq_ignore_ascii_case(hub.trim()))
            .ok_or(StatusCode::NOT_FOUND)?;
        let s = usize::try_from(*line)
            .ok()
            .and_then(|l| device.sensors.get(l))
            .ok_or(StatusCode::NOT_FOUND)?;
        return Ok(ResolvedProbe::from_sensor(device, s));
    }
    let stored = match &member_ref {
        MemberRef::Node(key) => match snapshot.hub_of_node(key) {
            Some((device, _)) => {
                let (host, key) = (device.hostname.clone(), key.clone());
                state
                    .db
                    .run(move |s| s.node_fields_with_readings(&host, &key))
                    .await
                    .unwrap_or_default()
            }
            None => Default::default(),
        },
        _ => Default::default(),
    };
    resolve_probe(&snapshot, &member_ref, &stored).ok_or(StatusCode::NOT_FOUND)
}

/// `[from, to)` for one of [`PROBE_RANGES`]. An unknown pill falls back to
/// 24 h rather than erroring — the value arrives from a query string.
fn range_bounds(range: &str, now: i64) -> (&'static str, i64, i64) {
    let (name, secs) = match range {
        "3d" => ("3d", 3 * 24 * 3600),
        "7d" => ("7d", 7 * 24 * 3600),
        _ => ("24h", 24 * 3600),
    };
    (name, now - secs, now)
}

// ---------------------------------------------------------------------
// GET /monitoring/probe/{token}
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct WindowQuery {
    pub(crate) range: Option<String>,
    /// Flash code from a manual event's redirect — a
    /// `monitoring.groups.err_*` slug, the same channel the dashboard used
    /// while this was a modal on it.
    error: Option<String>,
}

/// One field's aggregate line, pre-formatted.
///
/// The four statistics are rendered to strings here rather than in the
/// template because their precision is per unit (`decimals_for`: one place
/// for °C, none for %), and minijinja's `round` filter takes the places as
/// a literal — so a template would need a branch per unit to say the same
/// thing.
#[derive(Debug, Serialize)]
struct ProbeFieldCtx {
    field: Option<&'static str>,
    label: String,
    unit: String,
    color: &'static str,
    count: i64,
    min: Option<String>,
    max: Option<String>,
    avg: Option<String>,
    median: Option<String>,
}

/// One row of the window's event timeline.
///
/// `kind` is the dynamic half of `monitoring.probe_detail.event_{kind}` and
/// of the icon choice, so the five values here are a closed set the i18n
/// test enumerates.
#[derive(Debug, Serialize)]
struct ProbeEventCtx {
    /// Sort key and the chart markline's x position (unix seconds).
    ts: i64,
    /// RFC3339, for the `short_dt` filter in the row's tooltip.
    at: String,
    /// "před 5 min" — the row's visible time (lists are relative, the
    /// absolute goes in `title`; devices.node.md §1.3).
    at_rel: String,
    /// `value_raised` | `value_cleared` | `offline` | `online` | `manual`.
    kind: &'static str,
    /// `warning` / `critical` for the value classes, `None` otherwise —
    /// drives the row's tone and the markline's colour.
    severity: Option<String>,
    /// The alarm's own message, or the operator's text.
    detail: Option<String>,
    /// Value classes only — what the row says in words (§6.3 "lidský popis
    /// s hodnotou a mezí"): the alarm type, the quantity (`common.field.*`,
    /// set only for a multi-field Node/Beacon; a wired probe has one), and
    /// the value and limit with their unit. `value` is the triggering
    /// reading, so the template shows it on the raising edge only.
    alarm_type: Option<String>,
    field: Option<&'static str>,
    unit: Option<&'static str>,
    value: Option<f64>,
    threshold: Option<f64>,
    /// Manual events only: who wrote it, and the id the delete form posts.
    author: Option<String>,
    id: Option<i64>,
}

impl ProbeEventCtx {
    /// An event at `ts` with nothing else said yet.
    fn at(ts: i64, at: String) -> ProbeEventCtx {
        ProbeEventCtx {
            ts,
            at,
            at_rel: monitoring::relative_time(ts),
            kind: EVENT_MANUAL,
            severity: None,
            detail: None,
            alarm_type: None,
            field: None,
            unit: None,
            value: None,
            threshold: None,
            author: None,
            id: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct ProbeWindowCtx {
    token: String,
    /// See [`ResolvedProbe::page`].
    page: String,
    kind: &'static str,
    hostname: String,
    device_label: String,
    name: String,
    description: Option<String>,
    range: &'static str,
    ranges: Vec<&'static str>,
    from_iso: String,
    to_iso: String,
    /// Filename stem for the client-side PNG:
    /// `probe_{slug}_{range}_{yyyymmdd}.png` (decision 2026-09-08).
    png_filename: String,
    csv_href: String,
    device_href: Option<String>,
    history_href: Option<String>,
    operational_supported: bool,
    can_edit: bool,
    /// `datetime-local` default for the manual-event form: now, to the
    /// minute.
    now_local: String,
    fields: Vec<ProbeFieldCtx>,
    events: Vec<ProbeEventCtx>,
    /// One of the event reads failed — the list renders a "could not be
    /// read" line rather than passing an incomplete timeline off as a
    /// complete one (review item 8).
    events_unreadable: bool,
    /// The alarm walk gave up before reaching the start of the range
    /// (review item 5).
    events_truncated: bool,
    /// The probe was already offline (or disconnected) when the range
    /// started, and stayed that way — there is no edge to list, so the
    /// window says it in a line of its own (review item 7).
    ongoing_outage: bool,
    /// The chart island's JSON config, already `</`-escaped.
    chart_config: String,
    /// True when no field has a single stored point in the window — the
    /// template shows one "no data" line instead of five empty statistics.
    empty: bool,
}

/// One manual event as the Events card lists it (ui_components.md §5).
#[derive(Debug, Serialize)]
struct EventsCardRow {
    id: i64,
    /// RFC 3339, through `short_dt` in the template.
    at: String,
    text: String,
    author: String,
}

/// The device pages' Events card (ui_components.md §5, 2026-10-15): the
/// operator notes of one probe, newest first, and nothing else — alarms,
/// liveness and config changes live on their own surfaces. Reuses the
/// manual-event storage and the `/monitoring/probe/{token}/events` routes.
#[derive(Debug, Serialize)]
struct EventsCardCtx {
    token: String,
    /// The wired probe's Hub, which the routes need to resolve the line.
    hub: String,
    card_id: String,
    can_edit: bool,
    now_local: String,
    csv_name: String,
    events: Vec<EventsCardRow>,
    unreadable: bool,
}

/// The Events card's context for the probe `token` names (`node:{key}`,
/// `beacon:{MAC}`, `probe:{line}` with `hub`). `None` when it names
/// nothing, so the page simply leaves the card out.
pub(crate) async fn events_card(
    state: &AppState,
    current_user: &CurrentUser,
    token: &str,
    hub: Option<&str>,
) -> Option<minijinja::Value> {
    let probe = lookup_in(state, token, hub).await.ok()?;
    let (host, ident) = (probe.hostname.clone(), probe.probe.clone());
    let read = state
        .db
        .run(move |s| s.list_sensor_events(&host, &ident, 0, i64::MAX))
        .await;
    let unreadable = read.is_err();
    let mut events: Vec<EventsCardRow> = read
        .unwrap_or_default()
        .into_iter()
        .map(|e| EventsCardRow {
            id: e.id,
            at: format_rfc3339(e.ts),
            text: e.text,
            author: e.author,
        })
        .collect();
    events.reverse();
    let card_id = format!(
        "events-card-{}",
        token
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
    );
    Some(minijinja::Value::from_serialize(EventsCardCtx {
        csv_name: format!("events_{}_{}.csv", slug(&probe.name), today_yyyymmdd()),
        token: probe.token.clone(),
        hub: if probe.kind == "probe" {
            probe.hostname.clone()
        } else {
            String::new()
        },
        card_id,
        can_edit: render::can_edit_thresholds(current_user),
        now_local: crate::display_tz::datetime_local(render::now_unix()),
        events,
        unreadable,
    }))
}

/// `GET /monitoring/probe/{token}?range=` — one probe's detail page.
///
/// A PAGE since 2026-09-15, not the fragment that used to be swapped into a
/// modal on `/monitoring`. A wired probe is the one sensor kind with no
/// page of its own, while a Node and a Beacon each have one — so the same
/// click on the same list opened two different kinds of surface. The modal,
/// its opener and its pushState bookkeeping are gone from
/// pages/dashboard.html with it.
///
/// Gated with `CurrentUser` and nothing more, like MB2's two row fragments:
/// everything in it is already visible to a viewer on `/monitoring`,
/// `/history` and `/alarms`.
async fn probe_detail_page(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path(token): Path<String>,
    Query(q): Query<WindowQuery>,
) -> Response {
    let probe = match lookup(&state, &token).await {
        Ok(probe) => probe,
        Err(status) => {
            return (status, tr("monitoring.probe_detail.unknown_probe")).into_response()
        }
    };
    // A wired probe's page is on its Hub since 2026-10-14 (devices.hub.md
    // §3.2): the old address stays a bookmark that lands there.
    if probe.kind == "probe" {
        let mut url = probe.page.clone();
        if let Some(range) = q.range.as_deref().filter(|r| PROBE_RANGES.contains(r)) {
            url.push_str("?range=");
            url.push_str(range);
        }
        return Redirect::to(&url).into_response();
    }
    let w = probe_window(&state, &current_user, probe, &q).await;
    let ctx = window_page_ctx(
        &state,
        &current_user,
        w,
        &q,
        "dashboard",
        minijinja::context! {},
    )
    .await;
    render_page(&state, "pages/probe_detail.html", ctx)
}

/// The wired line `line` (0..7) of Hub `hub` as the probe page's `w`
/// context, for the per-probe page on the Hub (hub_routes). `None` when the
/// Hub or the line is unknown.
pub(crate) async fn line_window(
    state: &AppState,
    current_user: &CurrentUser,
    hub: &str,
    line: u8,
    q: &WindowQuery,
) -> Option<minijinja::Value> {
    let probe = lookup_in(state, &format!("probe:{line}"), Some(hub))
        .await
        .ok()?;
    Some(minijinja::Value::from_serialize(
        probe_window(state, current_user, probe, q).await,
    ))
}

/// The page context around a probe window: the Monitoring strip, the
/// flash error and the page's own `extra` keys.
pub(crate) async fn window_page_ctx<W: Serialize>(
    state: &AppState,
    current_user: &CurrentUser,
    w: W,
    q: &WindowQuery,
    nav: &str,
    extra: minijinja::Value,
) -> minijinja::Value {
    let error = q
        .error
        .as_deref()
        .filter(|c| !c.is_empty())
        .and_then(super::groups_error);

    // The Monitoring strip, like the page this one opens from: the fleet
    // row `routes::render` would pick for "dashboard" counts what Monitoring
    // does not show (monitoring.md §2.1, amended 2026-09-25).
    let portal_state = state.store.snapshot();
    let stats =
        crate::http::monitoring_stats(state, &portal_state, &state.offline.liveness_snapshot())
            .await;
    render::page_ctx(
        minijinja::context! {
            nav_active => nav,
            stats => stats,
            error => error,
            w => minijinja::Value::from_serialize(&w),
            ..extra
        },
        Some(current_user),
    )
}

/// One probe's window over the requested range.
async fn probe_window(
    state: &AppState,
    current_user: &CurrentUser,
    probe: ResolvedProbe,
    q: &WindowQuery,
) -> ProbeWindowCtx {
    let state = state.clone();
    let now = render::now_unix();
    let (range, from, to) = range_bounds(q.range.as_deref().unwrap_or("24h"), now);

    let mut fields = Vec::with_capacity(probe.fields.len());
    let mut any_points = false;
    for (i, f) in probe.fields.iter().enumerate() {
        let summary = summarize(&state, &probe, f, from, to).await;
        any_points |= summary.count > 0;
        let decimals = if f.unit == "%" { 0 } else { 1 };
        // Stored in the wire unit, shown in the displayed one (UX-038).
        let shown = node_display(&probe, f.name);
        let fmt = |v: Option<f64>| v.map(|v| format!("{:.decimals$}", shown(v)));
        fields.push(ProbeFieldCtx {
            field: f.name,
            label: f.label.clone(),
            unit: f.unit.clone(),
            color: SERIES_COLORS[i % SERIES_COLORS.len()],
            count: summary.count,
            min: fmt(summary.min),
            max: fmt(summary.max),
            avg: fmt(summary.avg),
            median: fmt(summary.median),
        });
    }

    let events = collect_events(&state, &probe, from, to).await;
    let chart_config = chart_config(&probe, &fields, from, to, &events.events);

    ProbeWindowCtx {
        token: probe.token.clone(),
        page: probe.page.clone(),
        kind: probe.kind,
        hostname: probe.hostname.clone(),
        device_label: probe.device_label.clone(),
        name: probe.name.clone(),
        description: probe.description.clone(),
        range,
        ranges: PROBE_RANGES.to_vec(),
        from_iso: format_rfc3339(from),
        to_iso: format_rfc3339(to),
        png_filename: format!(
            "probe_{}_{range}_{}.png",
            slug(&probe.name),
            today_yyyymmdd()
        ),
        csv_href: format!(
            "/export/probe/{}.csv?from={}&to={}",
            probe.token,
            format_rfc3339(from),
            format_rfc3339(to)
        ),
        device_href: probe.device_href.clone(),
        history_href: probe.history_href.clone(),
        operational_supported: probe.operational_supported,
        can_edit: render::can_edit_thresholds(current_user),
        now_local: crate::display_tz::datetime_local(now),
        fields,
        events_unreadable: events.unreadable,
        events_truncated: events.truncated,
        ongoing_outage: events.ongoing_outage,
        events: events.events,
        chart_config,
        empty: !any_points,
    }
}

/// One field's five statistics, from whichever table backs this probe kind.
async fn summarize(
    state: &AppState,
    probe: &ResolvedProbe,
    field: &ProbeField,
    from: i64,
    to: i64,
) -> FieldSummary {
    let result = match (&probe.probe, field.name) {
        (ProbeIdentity::Line(line), _) => {
            ({
                let a0 = probe.hostname.clone();
                let a1 = *line;
                state
                    .db
                    .run(move |s| s.summarize_minutes(&a0, a1, from, to))
            })
            .await
        }
        (ProbeIdentity::Node(key), Some(name)) => {
            ({
                let a0 = probe.hostname.clone();
                let a1 = key.clone();
                let a2 = name.to_string();
                state
                    .db
                    .run(move |s| s.summarize_node_field(&a0, &a1, &a2, from, to))
            })
            .await
        }
        (ProbeIdentity::Mac(mac), Some(name)) => match EyeField::from_field_name(name) {
            Some(column) => {
                ({
                    let a0 = probe.hostname.clone();
                    let a1 = mac.clone();
                    state
                        .db
                        .run(move |s| s.summarize_eye_field(&a0, &a1, column, from, to))
                })
                .await
            }
            None => Ok(FieldSummary::EMPTY),
        },
        _ => Ok(FieldSummary::EMPTY),
    };
    match result {
        Ok(summary) => summary,
        Err(err) => {
            // One field's failed summary shows as "no data" for that field
            // rather than failing the whole window: the chart, the events
            // and the other fields are still worth reading, and the log
            // line is the operator's channel for the failure.
            tracing::warn!(%err, token = probe.token, "probe summary failed");
            FieldSummary::EMPTY
        }
    }
}

/// The window's timeline plus the two things the list has to admit about
/// itself: that a read failed, and that the walk stopped early.
///
/// Neither degrades to silence. A failed read used to render as "no
/// events", which is the same picture a probe with a quiet week draws —
/// review item 8; the window now says "could not be read" instead.
#[derive(Default)]
struct EventList {
    events: Vec<ProbeEventCtx>,
    /// One of the three reads failed: the list on screen is incomplete and
    /// says so, rather than passing for an empty week.
    unreadable: bool,
    /// The alarm walk hit [`ALARM_MAX_PAGES`] before reaching `from`.
    truncated: bool,
    /// A liveness or disconnect outage was already open when the range
    /// started and produced no edge inside it (review item 7).
    ongoing_outage: bool,
}

/// The window's timeline: `alarm_events` (all three classes) plus
/// `sensor_events`, merged and sorted — **one read set, so the list and the
/// chart's marklines cannot disagree** (UI-M9).
///
/// The time window is applied **in Rust**, not in SQL. `triggered_at` and
/// `cleared_at` are RFC3339 strings written from device-supplied timestamps
/// that carry their own UTC offsets (`persist.rs`), so a lexicographic
/// `BETWEEN` in SQL would compare `+02:00` against `Z` as text and silently
/// drop or admit an hour's worth of events either side of the boundary.
/// `parse_iso8601` normalises the offset first. The hostname and sensor
/// filters *are* pushed into SQL, so the rows this scans are one probe's,
/// not the fleet's.
async fn collect_events(state: &AppState, probe: &ResolvedProbe, from: i64, to: i64) -> EventList {
    let mut list = EventList::default();

    let base = |offset: i64| {
        let mut filter = AlarmListFilter {
            limit: ALARM_PAGE_LIMIT,
            offset,
            hostname: Some(probe.hostname.clone()),
            ..Default::default()
        };
        match &probe.alarm_key {
            AlarmKey::Exact(id) => filter.sensor_id = Some(id.clone()),
            AlarmKey::Prefix(prefix) => filter.sensor_id_prefix = Some(prefix.clone()),
        }
        filter
    };

    // The walk (review item 5). Each page is the next `ALARM_PAGE_LIMIT`
    // rows of one probe's history, newest-string first; it stops as soon as
    // a page's last row is more than [`OFFSET_SKEW_S`] older than `from`,
    // and gives up — saying so — after [`ALARM_MAX_PAGES`].
    let mut walked = 0usize;
    'walk: loop {
        let filter = base((walked as i64) * ALARM_PAGE_LIMIT);
        let rows = match state.alarms.list(filter).await {
            Ok((rows, _)) => rows,
            Err(err) => {
                tracing::warn!(%err, token = probe.token, "probe window: alarm read failed");
                list.unreadable = true;
                break 'walk;
            }
        };
        let page_len = rows.len();
        for a in &rows {
            let class = classify(&a.alarm_type);
            let value = matches!(class, EventClass::Value);
            let field = value
                .then(|| crate::alarms::alarm_field(&a.sensor_id, a.sensor_name.as_deref()))
                .flatten();
            // The facts a value row describes itself with, shared by both
            // of its edges.
            // Value and limit in the displayed unit, as on the chart (UX-038).
            let shown = node_display(probe, field.map(|f| f.name));
            let facts = |e: ProbeEventCtx| ProbeEventCtx {
                alarm_type: value.then(|| a.alarm_type.clone()),
                field: field.filter(|_| probe.kind != "probe").map(|f| f.name),
                unit: match (
                    probe.kind,
                    field.and_then(|f| fiber_protocol::lorawan_field(f.name)),
                ) {
                    ("node", Some(f)) => f.display_unit(),
                    _ => field.and_then(|f| f.unit),
                },
                value: a.value.filter(|_| value).map(&shown),
                threshold: a.threshold.filter(|_| value).map(&shown),
                ..e
            };
            // One row, up to two events: the edge that opened it and,
            // for an outage that has ended, the edge that closed it.
            // That is exactly the one-row-per-outage decision showing
            // through — the recovery is a `cleared_at`, never a second
            // row (decision 2026-09-08).
            if let Ok(ts) = parse_iso8601(&a.triggered_at) {
                if ts >= from && ts < to {
                    list.events.push(facts(ProbeEventCtx {
                        kind: match class {
                            EventClass::Offline => EVENT_OFFLINE,
                            EventClass::Disconnect => EVENT_DISCONNECT,
                            EventClass::Value => EVENT_VALUE_RAISED,
                        },
                        severity: value.then(|| a.severity.clone()),
                        // A value row describes itself from its facts; its
                        // stored message is empty or English engine text.
                        detail: a.message.clone().filter(|_| !value),
                        ..ProbeEventCtx::at(ts, a.triggered_at.clone())
                    }));
                }
            }
            if let Some(ts) = a.cleared_at.as_deref().and_then(|s| parse_iso8601(s).ok()) {
                if ts >= from && ts < to {
                    list.events.push(facts(ProbeEventCtx {
                        // A `DISCONNECT` that ended and an `OFFLINE` that
                        // ended both read as "back": the recovery edge is
                        // the same fact whichever of the two noticed the
                        // silence, and the onset above already said which.
                        kind: match class {
                            EventClass::Offline | EventClass::Disconnect => EVENT_ONLINE,
                            EventClass::Value => EVENT_VALUE_CLEARED,
                        },
                        severity: value.then(|| a.severity.clone()),
                        ..ProbeEventCtx::at(ts, a.cleared_at.clone().unwrap_or_default())
                    }));
                }
            }
        }
        walked += 1;
        if page_len < ALARM_PAGE_LIMIT as usize {
            break 'walk; // the whole history, read.
        }
        let reached_the_start = rows
            .last()
            .and_then(|a| parse_iso8601(&a.triggered_at).ok())
            .is_some_and(|ts| ts < from - OFFSET_SKEW_S);
        if reached_the_start {
            break 'walk;
        }
        if walked >= ALARM_MAX_PAGES {
            tracing::warn!(
                token = probe.token,
                pages = walked,
                "probe window: alarm walk hit its page budget; the list is truncated"
            );
            list.truncated = true;
            break 'walk;
        }
    }

    // An outage that opened before `from` and has not closed produces no
    // edge inside the range at all, so without this the window of a probe
    // that has been silent for a week reads as "no events" (review item 7).
    // One cheap extra read: the probe's still-open rows, of which there are
    // at most a handful by construction (`active_alarm` keeps one per
    // `sensor_id`).
    let mut open = base(0);
    open.limit = 50;
    open.include_cleared = false;
    match state.alarms.list(open).await {
        Ok((rows, _)) => {
            list.ongoing_outage = rows.iter().any(|a| {
                matches!(
                    classify(&a.alarm_type),
                    EventClass::Offline | EventClass::Disconnect
                ) && parse_iso8601(&a.triggered_at).is_ok_and(|ts| ts < from)
            });
        }
        Err(err) => {
            tracing::warn!(%err, token = probe.token, "probe window: open-alarm read failed");
            list.unreadable = true;
        }
    }

    match ({
        let a0 = probe.hostname.clone();
        let a1 = probe.probe.clone();
        state
            .db
            .run(move |s| s.list_sensor_events(&a0, &a1, from, to))
    })
    .await
    {
        Ok(rows) => list.events.extend(rows.into_iter().map(|e| ProbeEventCtx {
            kind: EVENT_MANUAL,
            detail: Some(e.text),
            author: Some(e.author),
            id: Some(e.id),
            ..ProbeEventCtx::at(e.ts, format_rfc3339(e.ts))
        })),
        Err(err) => {
            tracing::warn!(%err, token = probe.token, "probe window: manual event read failed");
            list.unreadable = true;
        }
    }

    list.events.sort_by_key(|e| e.ts);
    list
}

/// The chart island's config, serialised here because minijinja is built
/// without its `json` feature (there is no `tojson` filter) — the same
/// reason and the same `</` escape `history_routes` uses: `</` cannot
/// appear literally inside a `<script>` element, even a JSON one, so a
/// probe named `</script>` would otherwise end the block early.
///
/// `range: "custom"` with explicit bounds: the window's `3d` pill has no
/// name any chart endpoint knows, and pinning `from`/`to` also means the
/// aggregates and the curve cover exactly the same seconds.
///
/// `thresholds: false` — the island draws threshold lines only for a single
/// series, and the window is multi-series by definition (T+H on one chart).
/// The bounds are on the device pages, where they can be edited.
fn chart_config(
    probe: &ResolvedProbe,
    fields: &[ProbeFieldCtx],
    from: i64,
    to: i64,
    events: &[ProbeEventCtx],
) -> String {
    let series: Vec<_> = probe
        .fields
        .iter()
        .zip(fields)
        .map(|(f, ctx)| {
            serde_json::json!({
                "url": f.chart_url,
                "name": f.name.map_or_else(|| f.label.clone(), |n| crate::i18n::field_label(crate::i18n::current_lang(), n, &f.label)),
                "color": ctx.color,
                "unit": f.unit,
                // The wired endpoint carries no hostname in its path, so the
                // series names the Hub it belongs to (devices.hub.md §12 E).
                // The Node and Beacon endpoints resolve it from the key and
                // ignore the parameter.
                "hub": probe.hostname,
            })
        })
        .collect();
    let events: Vec<_> = events
        .iter()
        .map(|e| {
            // The markline's label goes through a dynamic catalogue key, so
            // it is looked up only for a kind [`PROBE_EVENT_KINDS`] declares
            // — the set `every_probe_detail_dynamic_key_exists` proves both
            // catalogues cover. A kind added without an entry draws an
            // unlabelled line rather than printing the raw key across the
            // chart, which is what `i18n::lookup`'s fallback would do.
            let label = if PROBE_EVENT_KINDS.contains(&e.kind) {
                tr(&format!("monitoring.probe_detail.event_{}", e.kind))
            } else {
                String::new()
            };
            serde_json::json!({
                "ts": e.ts,
                "kind": e.kind,
                "severity": e.severity,
                "label": label,
            })
        })
        .collect();
    let config = serde_json::json!({
        "range": "custom",
        "agg": "auto",
        "from": from,
        "to": to,
        "thresholds": false,
        "series": series,
        "events": events,
    });
    config.to_string().replace("</", "<\\/")
}

// ---------------------------------------------------------------------
// GET /api/v1/chart/eye/{mac}/{field}
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct EyeChartQuery {
    pub(crate) range: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    agg: Option<String>,
}

#[derive(Serialize)]
struct EyeThresholdsJson {
    critical_low: Option<f64>,
    warning_low: Option<f64>,
    warning_high: Option<f64>,
    critical_high: Option<f64>,
    /// Always null: a Beacon's stored thresholds are the four-bound
    /// warning/critical range, with no separate alarm pair — the same shape
    /// and the same two nulls `chart_json_node` answers with.
    alarm_low: Option<f64>,
    alarm_high: Option<f64>,
}

#[derive(Serialize)]
struct EyeChartResponse {
    hostname: String,
    mac: String,
    field: String,
    granularity: &'static str,
    agg_effective: &'static str,
    points: Vec<ChartPoint>,
    thresholds: Option<EyeThresholdsJson>,
    sensor_name: Option<String>,
    unit: String,
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

/// `GET /api/v1/chart/eye/{mac}/{field}` — the Beacon chart endpoint that
/// did not exist before MB3.
///
/// `/history` deliberately offers no Beacon selection ("nothing serves
/// chart data for them yet", `history_routes`' picker comment), which is
/// exactly the gap this closes for the probe window — and which B6's
/// `beacon_detail.html` preview then reuses rather than adding a second
/// one.
///
/// **Temperature and humidity only.** `eye_readings` has two numeric
/// columns; `battery` and `movement` are not stored as a series at all, so
/// a request for either is a 404 rather than an empty chart that looks
/// broken.
async fn chart_json_eye(
    _current_user: CurrentUser,
    State(state): State<AppState>,
    Path((mac, field)): Path<(String, String)>,
    Query(q): Query<EyeChartQuery>,
) -> Response {
    // HS-17: MACs are stored uppercase; `7c:d9:…` in the URL was an "unknown beacon".
    let mac = mac.to_ascii_uppercase();
    let Some(column) = EyeField::from_field_name(&field) else {
        return json_error(StatusCode::NOT_FOUND, "unknown or unchartable field");
    };
    let f = fiber_protocol::eye_registry()
        .iter()
        .find(|f| f.name == field);
    let unit = f.and_then(|f| f.unit).unwrap_or("").to_string();

    let portal_state = state.store.snapshot();
    let Some((device, beacon)) = portal_state.hub_of_beacon(&mac) else {
        return json_error(StatusCode::NOT_FOUND, "unknown beacon");
    };
    let hostname = device.hostname.clone();
    let sensor_name = beacon.name.clone();

    let now = render::now_unix();
    let (from, to) =
        match super::history_routes::resolve_range_parts(q.range.as_deref(), q.from, q.to, now) {
            Ok(range) => range,
            Err(message) => return json_error(StatusCode::BAD_REQUEST, message),
        };

    let readings = match ({
        let a0 = hostname.clone();
        let a1 = mac.clone();
        state
            .db
            .run(move |s| s.query_eye_readings(&a0, &a1, from, to, None))
    })
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(%err, hostname, mac, field, "beacon chart query failed");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "query failed");
        }
    };

    // A stored Beacon reading is one scalar per field, so avg == min == max
    // and the island's range band collapses to zero width — the same shape
    // (and the same harmless consequence) `chart_json_node` documents.
    let points: Vec<ChartPoint> = readings
        .iter()
        .filter_map(|r| {
            let value = match column {
                EyeField::Temperature => r.temperature_c,
                EyeField::Humidity => r.humidity_pct.map(|h| h as f64),
            }?;
            Some(ChartPoint {
                ts: r.ts,
                avg: Some(value),
                min: Some(value),
                max: Some(value),
            })
        })
        .collect();
    let agg = super::history_routes::Agg::parse(q.agg.as_deref());
    let (points, agg_effective) = super::history_routes::apply_agg(points, agg, 1, to - from);

    let thresholds = ({
        let a0 = hostname.clone();
        let a1 = portal_storage::THRESHOLD_SCOPE_EYE.to_string();
        let a2 = mac.clone();
        state
            .db
            .run(move |s| s.list_field_thresholds(&a0, &a1, &a2))
    })
    .await
    .unwrap_or_default()
    .into_iter()
    .find(|t| t.field == field)
    .map(|t| EyeThresholdsJson {
        critical_low: t.critical_low,
        warning_low: t.warning_low,
        warning_high: t.warning_high,
        critical_high: t.critical_high,
        alarm_low: None,
        alarm_high: None,
    });

    Json(EyeChartResponse {
        hostname,
        mac,
        field,
        granularity: "raw",
        agg_effective,
        points,
        thresholds,
        sensor_name,
        unit,
    })
    .into_response()
}

// ---------------------------------------------------------------------
// GET /export/probe/{token_csv}
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ExportQuery {
    from: Option<String>,
    to: Option<String>,
}

/// `GET /export/probe/{token}.csv?from&to` — the window's range as a file.
///
/// Byte-identical header to `/export/group/{id}.csv` and
/// `/export/sensor/{line}.csv` (`CSV_HEADER`): one header convention across
/// every export, so an operator can open a probe file and a group file in
/// the same spreadsheet without reconciling columns.
///
/// The row fill is `group_routes::member_csv_rows` outright, handed a
/// synthesised member carrying **the window's own field list**. That is the
/// deliberate fix for the inconsistency §6 WP3.3 records: the group export
/// falls back to "every Continuous && chartable field" when a member has no
/// selection, which is not necessarily the set on screen. Here the exported
/// columns are by construction the ones the window charts and summarises.
///
/// Viewer-visible and audited as `export`, like the group export beside it.
async fn export_probe(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path(token_csv): Path<String>,
    Query(q): Query<ExportQuery>,
) -> Response {
    // matchit cannot mix a literal suffix into a `{param}` segment, so the
    // route captures the whole segment and this strips `.csv` — the same
    // trick `/export/group/{id}.csv` uses.
    let Some(token) = token_csv.strip_suffix(".csv") else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let probe = match lookup(&state, token).await {
        Ok(probe) => probe,
        Err(status) => return (status, "unknown probe").into_response(),
    };

    let (Some(from_raw), Some(to_raw)) = (q.from.as_deref(), q.to.as_deref()) else {
        return (StatusCode::BAD_REQUEST, "'from' and 'to' are required").into_response();
    };
    let (Some(from_ts), Some(to_ts)) = (
        crate::display_tz::parse_local(from_raw),
        crate::display_tz::parse_local(to_raw),
    ) else {
        return (StatusCode::BAD_REQUEST, "invalid date").into_response();
    };
    if to_ts <= from_ts {
        return (StatusCode::BAD_REQUEST, "'to' must be after 'from'").into_response();
    }
    if to_ts - from_ts > super::MAX_EXPORT_SPAN_S {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "export window is too wide; narrow it to 366 days or less",
        )
            .into_response();
    }

    let (line, node_key, mac) = probe.probe.columns();
    let member = SensorGroupMember {
        id: 0,
        group_id: 0,
        hostname: probe.hostname.clone(),
        sensor_line: line,
        node_key: node_key.map(str::to_string),
        mac: mac.map(str::to_string),
        fields: Some(
            probe
                .fields
                .iter()
                .filter_map(|f| f.name.map(str::to_string))
                .collect(),
        ),
        sort_order: 0,
    };

    // `[from, to)`, with no `+ 1`: the window hands this route the exact
    // bounds it summarised and charted (`from_iso`/`to_iso`), so an
    // inclusive upper bound here would put one sample in the file that is
    // in neither the curve nor the aggregates (review item 6). The group
    // export's `+ 1` stays where it is — its bounds come off a date picker,
    // not off a window that already drew them.
    //
    // One `db.run` for the whole file, as the group export does (#114).
    // Only a cancelled task lands in the `Err` arm; a failed query is
    // already logged and reported as `ok == false` inside.
    let (rows, ok) = state
        .db
        .run(move |s| Ok(member_csv_rows(s, &member, from_ts, to_ts)))
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, "probe export: database task failed");
            (String::new(), false)
        });
    let row_count = rows.lines().count();
    let mut csv = String::from(CSV_HEADER);
    csv.push_str(&rows);

    audit_action(
        &state,
        &current_user,
        "export",
        &probe.token,
        serde_json::json!({ "rows": row_count, "from": from_raw, "to": to_raw }),
        ok,
    )
    .await;

    let filename = format!("probe_{}_{}.csv", slug(&probe.name), today_yyyymmdd());
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        csv,
    )
        .into_response()
}

// ---------------------------------------------------------------------
// Manual events (manager+, audited)
// ---------------------------------------------------------------------

/// Back to the probe's own page on the same range — a full-page POST +
/// redirect leaves the operator exactly where they were. Until 2026-09-15
/// this went to `/?detail=…`, the dashboard modal's URL state; the detail
/// is a page now and the modal is gone. `?error=` carries a
/// `monitoring.groups.err_*` slug, which the page translates the same way
/// `dashboard_handler` does.
fn back(token: &str, range: &str, error: Option<&str>) -> Response {
    back_to(
        &format!("/monitoring/probe/{}", urlencoding_light(token)),
        range,
        error,
    )
}

/// [`back`] to a resolved probe's own page ([`ResolvedProbe::page`]).
fn back_to(page: &str, range: &str, error: Option<&str>) -> Response {
    let mut url = format!("{page}?range={range}");
    if let Some(error) = error {
        url.push_str("&error=");
        url.push_str(error);
    }
    Redirect::to(&url).into_response()
}

/// Percent-encodes the handful of characters a member token can carry that
/// must not travel raw in a query string. A token is `kind:identity` where
/// identity is hex, digits and colons (see `GroupRowCtx::token`), so this is
/// a short allow-list rather than a dependency.
fn urlencoding_light(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ':' => "%3A".to_string(),
            c if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' => c.to_string(),
            c => format!("%{:02X}", c as u32),
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct EventForm {
    /// `datetime-local`, so "YYYY-MM-DDTHH:MM" — backdating is allowed
    /// (monitoring.md §6.5).
    at: String,
    text: String,
    /// Which pill was open, so the redirect reopens the same range.
    #[serde(default)]
    range: String,
    /// The Hub of a wired probe, which `probe:{line}` does not name; the
    /// per-probe page on the Hub posts it.
    #[serde(default)]
    hub: String,
}

/// `POST /monitoring/probe/{token}/events` — one operator note on a probe.
///
/// Manager+, audited on every attempt. The audit `details` carry the probe,
/// the event time and the note's **length**, never its text: an audit row is
/// a record that something was written and by whom, and copying operator
/// prose into a second table would put the same free text in two places
/// with only one of them deletable.
async fn create_event(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(token): Path<String>,
    axum::Form(form): axum::Form<EventForm>,
) -> Response {
    let range = normalise_range(&form.range);
    let probe = match lookup_in(&state, &token, Some(&form.hub)).await {
        Ok(probe) => probe,
        // A token that names nothing is the operator's problem to see, not
        // a 500: the `MUTATING_ROUTES` sweep posts here with `probe:0`.
        Err(_) => return back(&token, range, Some("unknown_probe")),
    };

    let text = form.text.trim();
    if text.is_empty() {
        return back_to(&probe.page, range, Some("event_text_required"));
    }
    if text.chars().count() > EVENT_TEXT_MAX {
        return back_to(&probe.page, range, Some("event_text_too_long"));
    }
    let Some(ts) = crate::display_tz::parse_local(form.at.trim()) else {
        return back_to(&probe.page, range, Some("event_bad_time"));
    };

    let result = ({
        let a0 = NewSensorEvent {
            hostname: probe.hostname.clone(),
            probe: probe.probe.clone(),
            ts,
            text: text.to_string(),
            author: current_user.username.clone(),
            author_user_id: Some(current_user.id),
        };
        state.db.write(move |s| s.insert_sensor_event(&a0))
    })
    .await;

    audit_action(
        &state,
        &current_user,
        "probe_event_create",
        &probe.token,
        serde_json::json!({ "at": format_rfc3339(ts), "chars": text.chars().count() }),
        result.is_ok(),
    )
    .await;

    match result {
        Ok(_) => back_to(&probe.page, range, None),
        Err(err) => {
            tracing::warn!(%err, token = probe.token, "manual event insert failed");
            back_to(&probe.page, range, Some("event_save_failed"))
        }
    }
}

/// `POST /monitoring/probe/{token}/events/{id}/delete`.
///
/// No edit route exists by design (§6.5): a correction is a delete plus a
/// new row, which keeps both acts in the audit trail. `success` is
/// `Ok(true)` — deleting a row that was not there is audited as an
/// unsuccessful delete rather than reported as an error, the same stance
/// `delete_group` takes.
async fn delete_event(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path((token, id)): Path<(String, i64)>,
    axum::Form(form): axum::Form<DeleteForm>,
) -> Response {
    let range = normalise_range(&form.range);

    // The probe is resolved, not merely parsed, because the ownership check
    // needs its **hostname**: `sensor_line` 1 exists on every Hub, so
    // comparing only the identity would let a manager looking at Hub A's
    // line 1 delete Hub B's note by editing the id in the form (review
    // item 1). `create_event` already resolves the same way.
    let probe = match lookup_in(&state, &token, Some(&form.hub)).await {
        Ok(probe) => probe,
        Err(_) => return back(&token, range, Some("unknown_probe")),
    };

    // Read first, so the audit row can say whether the id even named one of
    // this probe's rows; the delete itself is scoped again in SQL, so the
    // check and the statement cannot drift apart.
    let existing = state.db.run(move |s| s.get_sensor_event(id)).await;
    let owned = matches!(
        &existing,
        Ok(Some(event)) if event.hostname == probe.hostname && event.probe == probe.probe
    );

    let result = if owned {
        ({
            let a0 = probe.hostname.clone();
            let a1 = probe.probe.clone();
            state.db.write(move |s| s.delete_sensor_event(&a0, &a1, id))
        })
        .await
    } else {
        Ok(false)
    };
    // `success` is `Ok(true)` and nothing else: a delete of a row that was
    // not there — or not this probe's — is audited as an unsuccessful
    // delete rather than reported as an error, the same stance
    // `delete_group` takes.
    let success = matches!(result, Ok(true));

    audit_action(
        &state,
        &current_user,
        "probe_event_delete",
        &probe.token,
        serde_json::json!({ "event_id": id, "matched": owned }),
        success,
    )
    .await;

    if let Err(err) = &result {
        tracing::warn!(%err, token = probe.token, id, "manual event delete failed");
        return back_to(&probe.page, range, Some("event_save_failed"));
    }
    back_to(&probe.page, range, None)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DeleteForm {
    range: String,
    /// See [`EventForm::hub`].
    #[serde(default)]
    hub: String,
}

/// A range that came off a form, narrowed to one of [`PROBE_RANGES`].
fn normalise_range(raw: &str) -> &'static str {
    PROBE_RANGES
        .into_iter()
        .find(|r| *r == raw)
        .unwrap_or("24h")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_range_pills_resolve_to_their_own_windows() {
        let now = 1_768_314_600;
        assert_eq!(range_bounds("24h", now), ("24h", now - 86_400, now));
        assert_eq!(range_bounds("3d", now), ("3d", now - 3 * 86_400, now));
        assert_eq!(range_bounds("7d", now), ("7d", now - 7 * 86_400, now));
        // `30d` is a `/history` pill, not one of ours: a value off a query
        // string falls back rather than erroring, and 7 d is the window's
        // documented maximum (monitoring.md §6.1).
        assert_eq!(range_bounds("30d", now), ("24h", now - 86_400, now));
        assert_eq!(range_bounds("", now), ("24h", now - 86_400, now));
    }

    #[test]
    fn a_form_range_is_narrowed_to_the_pill_set() {
        assert_eq!(normalise_range("7d"), "7d");
        assert_eq!(normalise_range("30d"), "24h");
        assert_eq!(normalise_range("../../etc"), "24h");
    }

    /// The redirect target has to survive a Beacon token, which is full of
    /// colons — raw, they would still parse, but percent-encoding is what
    /// keeps the value a value rather than something a proxy may rewrite.
    #[test]
    fn a_beacon_token_survives_the_redirect_query() {
        assert_eq!(
            urlencoding_light("beacon:AA:BB:01"),
            "beacon%3AAA%3ABB%3A01"
        );
        assert_eq!(urlencoding_light("probe:0"), "probe%3A0");
        assert_eq!(urlencoding_light("node:70b3d5"), "node%3A70b3d5");
    }

    /// The window's two dynamic key families, in **both** catalogues.
    ///
    /// `every_static_template_key_exists` cannot see either: the template
    /// builds them as `("monitoring.probe_detail.event_" ~ e.kind)|t` and
    /// `("monitoring.probe_detail.range_" ~ r)|t`, so a missing entry would
    /// put the raw key on screen and fail nothing. This is the test that
    /// stands in for it, enumerating the two closed sets the code itself
    /// declares rather than a list copied by hand.
    #[test]
    fn every_probe_detail_dynamic_key_exists() {
        let mut keys: Vec<String> = PROBE_EVENT_KINDS
            .iter()
            .map(|k| format!("monitoring.probe_detail.event_{k}"))
            .collect();
        keys.extend(
            PROBE_RANGES
                .iter()
                .map(|r| format!("monitoring.probe_detail.range_{r}")),
        );
        // The liveness rows also reach `ui.alarm_type_label`'s dynamic
        // `alarms.type.{lower}` lookup, which is where an unlabelled
        // `OFFLINE` would surface as the raw enum.
        keys.push("alarms.type.offline".to_string());

        for key in keys {
            for lang in crate::i18n::LANGS {
                assert_ne!(
                    crate::i18n::lookup(lang, &key),
                    key,
                    "{key} is missing from the {lang} catalogue"
                );
            }
        }

        // The Czech wording the spec's wireframe uses verbatim (§6, the
        // "⛔ sonda offline / ✓ sonda online" row).
        assert_eq!(
            crate::i18n::lookup("cs", "monitoring.probe_detail.event_offline"),
            "sonda offline"
        );
        assert_eq!(
            crate::i18n::lookup("cs", "monitoring.probe_detail.event_online"),
            "návrat online"
        );
    }

    mod integration {
        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use fiber_protocol::payloads::sensors_aggregated::{
            AggregatedSensor, SensorsAggregatedMsg, TemperatureStats,
        };
        use fiber_protocol::DeviceStatus;
        use tower::ServiceExt;

        use crate::auth::{Role, SESSION_COOKIE};
        use crate::http::AppState;
        use crate::routes::test_support::{login_as_role, test_state};
        use crate::state::events::{IngestEvent, NodeReadingMsg};

        const HOST: &str = "h1";
        const NODE: &str = "n1";
        const MAC: &str = "AA:BB:CC:DD:EE:01";

        /// One Hub reporting all three probe kinds: a wired line, a Node
        /// with two fields, and a Beacon with temperature and humidity.
        async fn state_with_every_kind() -> (AppState, tempfile::TempDir) {
            let (state, dir) = test_state().await;
            state.store.apply(IngestEvent::Status {
                hostname: HOST.into(),
                status: DeviceStatus::Online,
            });
            state.store.apply(IngestEvent::SensorsAggregated {
                hostname: HOST.into(),
                payload: SensorsAggregatedMsg {
                    timestamp: Some("2026-01-13T14:30:00Z".into()),
                    period_start_ts: None,
                    period_end_ts: None,
                    duration_sec: Some(60),
                    sensors: vec![AggregatedSensor {
                        line: 1,
                        name: Some("Cold box".into()),
                        location: Some("Warehouse B".into()),
                        sample_count: 60,
                        disconnected_count: 0,
                        temperature: Some(TemperatureStats {
                            min_celsius: Some(3.5),
                            max_celsius: Some(4.5),
                            avg_celsius: Some(4.0),
                        }),
                        alarm_counts: None,
                        dominant_alarm_state: "NORMAL".into(),
                        alarm_triggered_at: None,
                    }],
                },
            });
            state.store.apply(IngestEvent::NodeReading {
                hostname: HOST.into(),
                node_key: NODE.into(),
                payload: Box::new(NodeReadingMsg {
                    message_id: "m-n1".into(),
                    dev_eui: NODE.into(),
                    serial: Some("SN-n1".into()),
                    fields: [
                        ("temperature".to_string(), 4.0),
                        ("humidity".to_string(), 50.0),
                    ]
                    .into_iter()
                    .collect(),
                    counters: Default::default(),
                    events: Vec::new(),
                    gateways: Vec::new(),
                    rssi: Some(-72),
                    snr: Some(8.25),
                    fport: None,
                    page_index: None,
                    page_count: None,
                    dr: None,
                    received_at: "2026-01-13T14:30:00Z".to_string(),
                }),
            });
            state.store.apply(IngestEvent::EyeSensors {
                hostname: HOST.into(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: false,
                    tags: vec![fiber_protocol::payloads::eye::EyeTag {
                        gateway: None,
                        mac: MAC.into(),
                        name: Some("Freezer".into()),
                        temperature_c: Some(-18.0),
                        humidity_pct: Some(40),
                        battery_mv: None,
                        low_battery: None,
                        magnet_present: None,
                        magnet_detected: None,
                        moving: None,
                        movement_count: None,
                        pitch_deg: None,
                        roll_deg: None,
                        rssi: None,
                        last_seen_ts: None,
                        stale: None,
                        provisioning: None,
                        discovered: None,
                        field_alarm_states: std::collections::HashMap::new(),
                        alarm_state: Some("NORMAL".into()),
                        ble_config: None,
                    }],
                },
            });
            (state, dir)
        }

        fn side_door(dir: &tempfile::TempDir) -> portal_storage::Storage {
            portal_storage::Storage::open(&dir.path().join("portal.db"), None).expect("open")
        }

        async fn get(router: &axum::Router, path: &str, token: &str) -> axum::response::Response {
            router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .expect("response")
        }

        async fn post(
            router: &axum::Router,
            path: &str,
            token: &str,
            body: &str,
        ) -> axum::response::Response {
            router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .expect("response")
        }

        async fn body_string(response: axum::response::Response) -> String {
            let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
                .await
                .expect("body");
            String::from_utf8_lossy(&bytes).into_owned()
        }

        async fn audit_rows(state: &AppState, event_type: &str) -> Vec<portal_storage::AuditEntry> {
            state
                .auth
                .audit_page(portal_storage::AuditFilter {
                    event_type: Some(event_type.into()),
                    username: None,
                    since: None,
                    until: None,
                    success: None,
                    search: None,
                    limit: 50,
                    offset: 0,
                    ..Default::default()
                })
                .await
                .expect("audit page")
                .0
        }

        /// A Node's temperature can be in group A and its humidity in group
        /// B (MB1's per-field exclusivity), and the window is addressed by
        /// the probe with no group id at all — so it shows **both**,
        /// whichever row was clicked.
        #[tokio::test]
        async fn the_window_renders_every_field_of_the_probe_regardless_of_group() {
            let (state, _dir) = state_with_every_kind().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            let router = super::super::router().with_state(state.clone());

            // One group holding only the Node's temperature.
            ({
                let a0 = "Only temperature".to_string();
                let a2 = vec![portal_storage::NewSensorGroupMember {
                    hostname: HOST.into(),
                    sensor_line: None,
                    node_key: Some(NODE.into()),
                    mac: None,
                    fields: Some(vec!["temperature".into()]),
                }];
                state
                    .db
                    .write(move |s| s.create_sensor_group(&a0, None, &a2))
            })
            .await
            .expect("create group");

            let html =
                body_string(get(&router, &format!("/monitoring/probe/node:{NODE}"), &viewer).await)
                    .await;
            assert!(html.contains("Temperature"), "{html}");
            assert!(
                html.contains("Humidity"),
                "the window is probe-scoped, not group-scoped: {html}"
            );
            // Its chart series are the same two fields.
            assert!(html.contains(&format!("/api/v1/chart/node/{NODE}/temperature")));
            assert!(html.contains(&format!("/api/v1/chart/node/{NODE}/humidity")));
        }

        /// The probe page sits under the Monitoring nav entry, so it carries
        /// the Monitoring strip: grouped probes (monitoring.md §2.1, amended
        /// 2026-09-25), refetched from `/stats/monitoring`, not the fleet
        /// row under the same labels.
        #[tokio::test]
        async fn the_probe_page_carries_the_monitoring_strip() {
            let (state, _dir) = state_with_every_kind().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            let router = super::super::router().with_state(state.clone());
            ({
                let name = "Only the Node".to_string();
                let members = vec![portal_storage::NewSensorGroupMember {
                    hostname: HOST.into(),
                    sensor_line: None,
                    node_key: Some(NODE.into()),
                    mac: None,
                    fields: None,
                }];
                state
                    .db
                    .write(move |s| s.create_sensor_group(&name, None, &members))
            })
            .await
            .expect("create group");

            let html =
                body_string(get(&router, &format!("/monitoring/probe/node:{NODE}"), &viewer).await)
                    .await;
            let start = html.find(r#"id="page-stats""#).expect("the strip");
            let strip = &html[start..start + html[start..].find('>').expect("tag end")];
            assert!(
                strip.contains(r#"hx-get="&#x2f;stats&#x2f;monitoring""#),
                "the Monitoring scope's refetch: {strip}"
            );
        }

        /// §6's *Accept:* — "the window's aggregates match a hand-computed
        /// control set over a seeded range, median included".
        #[tokio::test]
        async fn the_window_aggregates_match_a_hand_computed_control_set() {
            let (state, dir) = state_with_every_kind().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            let router = super::super::router().with_state(state.clone());
            let storage = side_door(&dir);

            // Five readings inside the last hour: sorted 1, 3, 4, 7, 10 ->
            // count 5, min 1, max 10, avg 5, median 4.
            let now = crate::render::now_unix();
            for (i, v) in [7.0_f64, 1.0, 10.0, 3.0, 4.0].into_iter().enumerate() {
                storage
                    .insert_field_reading(&portal_storage::NodeFieldReading {
                        hostname: HOST.into(),
                        node_key: NODE.into(),
                        field: "temperature".into(),
                        ts: now - 3_000 + i as i64 * 60,
                        value: Some(v),
                        alarm_state: None,
                        source: portal_storage::SOURCE_LIVE.into(),
                    })
                    .expect("insert");
            }

            let html =
                body_string(get(&router, &format!("/monitoring/probe/node:{NODE}"), &viewer).await)
                    .await;
            assert!(html.contains("5 samples"), "count: {html}");
            assert!(html.contains("min 1.0"), "min: {html}");
            assert!(html.contains("max 10.0"), "max: {html}");
            assert!(html.contains("avg 5.0"), "avg: {html}");
            assert!(html.contains("med 4.0"), "median: {html}");
        }

        /// UX-039: right after a start the live state has no fields for a
        /// Node until its next uplink; stored readings still put the field
        /// (and its history) on the page.
        #[tokio::test]
        async fn a_node_with_no_live_fields_yet_shows_its_stored_ones() {
            let (state, dir) = test_state().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            let node = "70b3d50000000009";
            state.store.apply(IngestEvent::NodeReading {
                hostname: HOST.into(),
                node_key: node.into(),
                payload: Box::new(NodeReadingMsg {
                    message_id: "m-n9".into(),
                    dev_eui: node.into(),
                    serial: None,
                    fields: Default::default(),
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
            side_door(&dir)
                .insert_field_reading(&portal_storage::NodeFieldReading {
                    hostname: HOST.into(),
                    node_key: node.into(),
                    field: "temperature".into(),
                    ts: crate::render::now_unix() - 600,
                    value: Some(4.2),
                    alarm_state: None,
                    source: portal_storage::SOURCE_LIVE.into(),
                })
                .expect("insert");
            let router = super::super::router().with_state(state.clone());
            let html =
                body_string(get(&router, &format!("/monitoring/probe/node:{node}"), &viewer).await)
                    .await;
            assert!(
                html.contains(&format!("/api/v1/chart/node/{node}/temperature")),
                "{html}"
            );
            assert!(
                html.contains("1 samples") || html.contains("1 sample"),
                "{html}"
            );
        }

        /// §6.5's promise, and the whole reason `sensor_events` is keyed by
        /// probe identity rather than by group membership.
        #[tokio::test]
        async fn a_manual_event_survives_its_probe_moving_between_groups() {
            let (state, _dir) = state_with_every_kind().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());

            let group_a = ({
                let a0 = "A".to_string();
                let a2 = vec![portal_storage::NewSensorGroupMember {
                    hostname: HOST.into(),
                    sensor_line: None,
                    node_key: Some(NODE.into()),
                    mac: None,
                    fields: None,
                }];
                state
                    .db
                    .write(move |s| s.create_sensor_group(&a0, None, &a2))
            })
            .await
            .expect("create A");

            let at = crate::routes::history_routes::format_rfc3339(crate::render::now_unix() - 600);
            let created = post(
                &router,
                &format!("/monitoring/probe/node:{NODE}/events"),
                &manager,
                &format!(
                    "at={}&text=battery+replaced&range=24h",
                    &at[..16].replace(':', "%3A")
                ),
            )
            .await;
            assert_eq!(created.status(), StatusCode::SEE_OTHER);

            // Move the probe: delete group A, put it in a new group B.
            state
                .db
                .write(move |s| s.delete_sensor_group(group_a))
                .await
                .expect("delete A");
            ({
                let a0 = "B".to_string();
                let a2 = vec![portal_storage::NewSensorGroupMember {
                    hostname: HOST.into(),
                    sensor_line: None,
                    node_key: Some(NODE.into()),
                    mac: None,
                    fields: None,
                }];
                state
                    .db
                    .write(move |s| s.create_sensor_group(&a0, None, &a2))
            })
            .await
            .expect("create B");

            let html = body_string(
                get(&router, &format!("/monitoring/probe/node:{NODE}"), &manager).await,
            )
            .await;
            assert!(
                html.contains("battery replaced"),
                "the event follows its probe: {html}"
            );
        }

        /// UI-M10: "audited on create and delete".
        #[tokio::test]
        async fn a_manual_event_create_and_delete_each_leave_an_audit_row() {
            let (state, _dir) = state_with_every_kind().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());

            post(
                &router,
                "/monitoring/probe/probe:1/events",
                &manager,
                "at=2026-01-13T14%3A30&text=sensor+maintenance&range=24h",
            )
            .await;

            let created = audit_rows(&state, "probe_event_create").await;
            assert_eq!(created.len(), 1);
            assert!(created[0].success);
            assert_eq!(created[0].target.as_deref(), Some("probe:1"));
            let details = created[0].details.as_deref().expect("details");
            assert!(details.contains("\"chars\":18"), "{details}");
            assert!(
                !details.contains("sensor maintenance"),
                "the audit row records that a note was written, not its prose: {details}"
            );

            let id = ({
                let a0 = HOST.to_string();
                let a1 = portal_storage::ProbeIdentity::Line(1);
                let a3 = i64::MAX;
                state.db.run(move |s| s.list_sensor_events(&a0, &a1, 0, a3))
            })
            .await
            .expect("list")[0]
                .id;

            let deleted = post(
                &router,
                &format!("/monitoring/probe/probe:1/events/{id}/delete"),
                &manager,
                "range=24h",
            )
            .await;
            assert_eq!(deleted.status(), StatusCode::SEE_OTHER);
            let rows = audit_rows(&state, "probe_event_delete").await;
            assert_eq!(rows.len(), 1);
            assert!(rows[0].success, "success is Ok(true), the real outcome");

            // Deleting it again is an audited failure, not an error.
            post(
                &router,
                &format!("/monitoring/probe/probe:1/events/{id}/delete"),
                &manager,
                "range=24h",
            )
            .await;
            let rows = audit_rows(&state, "probe_event_delete").await;
            assert_eq!(rows.len(), 2);
            assert!(!rows[0].success);
        }

        /// The refusal is proven with a real request, not by looking at the
        /// template (devices.backend.md §3) — and it must leave no trail
        /// and no row, because nothing happened.
        #[tokio::test]
        async fn a_viewer_is_refused_both_event_mutations_and_leaves_no_trail() {
            let (state, _dir) = state_with_every_kind().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            let router = super::super::router().with_state(state.clone());

            let created = post(
                &router,
                "/monitoring/probe/probe:1/events",
                &viewer,
                "at=2026-01-13T14%3A30&text=nope&range=24h",
            )
            .await;
            assert_eq!(created.status(), StatusCode::FORBIDDEN);

            let deleted = post(
                &router,
                "/monitoring/probe/probe:1/events/1/delete",
                &viewer,
                "range=24h",
            )
            .await;
            assert_eq!(deleted.status(), StatusCode::FORBIDDEN);

            assert!(audit_rows(&state, "probe_event_create").await.is_empty());
            assert!(audit_rows(&state, "probe_event_delete").await.is_empty());
            assert!(({
                let a0 = HOST.to_string();
                let a1 = portal_storage::ProbeIdentity::Line(1);
                let a3 = i64::MAX;
                state.db.run(move |s| s.list_sensor_events(&a0, &a1, 0, a3))
            })
            .await
            .expect("list")
            .is_empty());
        }

        /// Reading is viewer work throughout (monitoring.md §8): the window
        /// and its export are both `CurrentUser`, like the group export
        /// beside them.
        #[tokio::test]
        async fn a_viewer_can_open_the_window_and_download_its_csv() {
            let (state, _dir) = state_with_every_kind().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            let router = super::super::router().with_state(state.clone());

            let window = get(&router, "/hubs/h1/sensors/line-2", &viewer).await;
            assert_eq!(window.status(), StatusCode::OK);
            let html = body_string(window).await;
            // ...and sees no way to write one.
            assert!(!html.contains("probe-event-form"), "{html}");

            let csv = get(
                &router,
                "/export/probe/probe:1.csv?from=2026-01-13T00:00&to=2026-01-14T00:00",
                &viewer,
            )
            .await;
            assert_eq!(csv.status(), StatusCode::OK);
            // The filename carries today's date, so the stem is what is
            // asserted: the probe's slugged **name**, not its token. This
            // fixture sends no `config/state`; the name is the one its
            // telemetry window repeats ("Cold Box") — the point is that the
            // export is named after what the operator sees.
            let disposition = csv
                .headers()
                .get(axum::http::header::CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            assert!(
                disposition.contains("filename=\"probe_cold-box_"),
                "{disposition}"
            );
            let rows = audit_rows(&state, "export").await;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].target.as_deref(), Some("probe:1"));
        }

        /// One header convention across every export (§6.4 / WP3.3): a
        /// probe file and a group file open in the same spreadsheet.
        #[tokio::test]
        async fn the_probe_csv_header_is_byte_identical_to_the_group_export() {
            let (state, _dir) = state_with_every_kind().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            let router = super::super::router().with_state(state.clone());

            let csv = body_string(
                get(
                    &router,
                    "/export/probe/probe:1.csv?from=2026-01-13T00:00&to=2026-01-14T00:00",
                    &viewer,
                )
                .await,
            )
            .await;
            assert!(csv.starts_with(super::super::CSV_HEADER));
        }

        /// Both CSV exports materialise every row twice before writing a
        /// byte, over statements that carry no `LIMIT`. An unbounded window
        /// was a viewer-reachable way to OOM the process.
        #[tokio::test]
        async fn an_export_window_wider_than_a_year_is_refused() {
            let (state, _dir) = state_with_every_kind().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            let router = super::super::router().with_state(state.clone());

            let wide = get(
                &router,
                "/export/probe/probe:1.csv?from=1970-01-01T00:00&to=2099-01-01T00:00",
                &viewer,
            )
            .await;
            assert_eq!(wide.status(), StatusCode::UNPROCESSABLE_ENTITY);

            // The boundary itself still answers, so a legitimate year-long
            // export is untouched.
            let year = get(
                &router,
                "/export/probe/probe:1.csv?from=2025-01-13T00:00&to=2026-01-13T00:00",
                &viewer,
            )
            .await;
            assert_eq!(year.status(), StatusCode::OK);
        }

        /// The `MUTATING_ROUTES` sweep posts to these paths as a manager
        /// with a token that resolves to nothing, so a 500 here would fail
        /// the whole suite — and a real operator whose probe has just gone
        /// off the network deserves the page back, not a stack trace.
        #[tokio::test]
        async fn an_unknown_probe_token_is_404_not_500() {
            let (state, _dir) = state_with_every_kind().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());

            // `probe:99` and not `probe:7`: a Hub's eight wired lines are a
            // fixed array in `DeviceState`, so every line 0-7 resolves
            // whether or not anything is wired to it — the picker offers all
            // eight for the same reason. Only a line outside that range is a
            // genuine miss.
            for path in [
                "/monitoring/probe/probe:99",
                "/monitoring/probe/node:nope",
                "/monitoring/probe/beacon:FF:FF:FF:FF:FF:FF",
            ] {
                assert_eq!(
                    get(&router, path, &manager).await.status(),
                    StatusCode::NOT_FOUND,
                    "{path}"
                );
            }
            // A token that does not even parse is a 400, not a 404: there
            // is nothing to look up.
            assert_eq!(
                get(&router, "/monitoring/probe/nonsense", &manager)
                    .await
                    .status(),
                StatusCode::BAD_REQUEST
            );
            assert_eq!(
                get(
                    &router,
                    "/export/probe/probe:99.csv?from=2026-01-13T00:00&to=2026-01-14T00:00",
                    &manager
                )
                .await
                .status(),
                StatusCode::NOT_FOUND
            );

            // The two mutations redirect rather than erroring.
            let created = post(
                &router,
                "/monitoring/probe/probe:99/events",
                &manager,
                "at=2026-01-13T14%3A30&text=x&range=24h",
            )
            .await;
            assert_eq!(created.status(), StatusCode::SEE_OTHER);
            assert!(created
                .headers()
                .get(axum::http::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|l| l.contains("error=unknown_probe")));
        }

        /// §9.2's decision, made visible: a wired probe has no per-probe
        /// liveness, so the window says so instead of showing an empty
        /// operational section that reads as "never offline".
        #[tokio::test]
        async fn the_window_says_a_wired_probe_has_no_operational_events() {
            let (state, _dir) = state_with_every_kind().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            let router = super::super::router().with_state(state.clone());

            // The apostrophe in the catalogue string is HTML-escaped on the
            // way out, so the assertion is on a fragment that has none.
            const SAID: &str = "Liveness of a wired probe is the Hub";

            let wired = wired_timeline(&state, 1).await;
            assert!(wired.contains(SAID), "{wired}");

            let node =
                body_string(get(&router, &format!("/monitoring/probe/node:{NODE}"), &viewer).await)
                    .await;
            assert!(
                !node.contains(SAID),
                "a Node does have its own liveness, so it gets the real event list"
            );
        }

        /// One `alarm_events` row for a wired line, ready to seed.
        fn alarm(
            alarm_type: &str,
            triggered_at: i64,
            cleared_at: Option<i64>,
            message: &str,
        ) -> portal_storage::NewAlarmEvent {
            use crate::routes::history_routes::format_rfc3339;
            portal_storage::NewAlarmEvent {
                hostname: HOST.into(),
                sensor_id: "line-1".into(),
                sensor_name: Some("Cold box".into()),
                sensor_location: Some("Warehouse B".into()),
                alarm_type: alarm_type.into(),
                severity: "critical".into(),
                from_state: None,
                to_state: None,
                message: Some(message.into()),
                value: None,
                threshold: None,
                triggered_at: format_rfc3339(triggered_at),
                cleared_at: cleared_at.map(format_rfc3339),
                acknowledged_at: None,
                acknowledged_by_user_id: None,
                acknowledged_by_username: None,
                native_key: None,
            }
        }

        /// A Beacon registered on one Hub and overheard as a stranger by
        /// another (a cluster Client in range) opens on its owner. The
        /// stranger Hub's hostname sorts first, which used to answer
        /// "unknown probe" for a Beacon the Monitoring row links to.
        #[tokio::test]
        async fn a_stranger_copy_on_another_hub_does_not_shadow_the_owner() {
            let (state, _dir) = state_with_every_kind().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "a-client".into(),
                status: DeviceStatus::Online,
            });
            state.store.apply(IngestEvent::EyeSensors {
                hostname: "a-client".into(),
                payload: serde_json::from_value(serde_json::json!({
                    "timestamp": null,
                    "tags": [{ "mac": MAC, "discovered": true }],
                }))
                .unwrap(),
            });
            let router = super::super::router().with_state(state.clone());

            for path in [
                format!("/monitoring/probe/beacon:{MAC}"),
                format!("/monitoring/probe/beacon:{}", MAC.to_lowercase()),
            ] {
                let page = get(&router, &path, &viewer).await;
                assert_eq!(page.status(), StatusCode::OK, "{path}");
                let body = body_string(page).await;
                assert!(body.contains("Freezer"), "{body}");
            }
        }

        /// monitoring.md §6's footer rule, clarified 2026-09-10: "Device
        /// detail →" renders wherever a device page exists, and B5 built
        /// `/beacons/{mac}`. A wired line still has none, and "Open in Trends
        /// →" is still omitted for a Beacon (`/history` cannot select one).
        #[tokio::test]
        async fn the_footer_links_where_a_device_page_actually_exists() {
            let (state, _dir) = state_with_every_kind().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            let router = super::super::router().with_state(state.clone());

            let beacon = body_string(
                get(&router, &format!("/monitoring/probe/beacon:{MAC}"), &viewer).await,
            )
            .await;
            // minijinja escapes `/` in an attribute as `&#x2f;`, so the
            // assertion is on the escaped href the browser actually gets.
            assert!(
                beacon.contains(&format!("href=\"&#x2f;beacons&#x2f;{MAC}&#x2f;overview\"")),
                "{beacon}"
            );
            assert!(
                !beacon.contains("history?s="),
                "no Beacon selection exists on /history: {beacon}"
            );

            let node =
                body_string(get(&router, &format!("/monitoring/probe/node:{NODE}"), &viewer).await)
                    .await;
            assert!(
                node.contains(&format!("href=\"&#x2f;nodes&#x2f;{NODE}&#x2f;overview\"")),
                "{node}"
            );

            let wired = body_string(get(&router, "/hubs/h1/sensors/line-2", &viewer).await).await;
            assert!(
                !wired.contains("Device detail"),
                "a wired line has no device page: {wired}"
            );
        }

        /// Review item 18: the window's event list, read through the route
        /// rather than through `collect_events`.
        ///
        /// Three things at once, because they are one read set: each alarm
        /// class lands under **its own** kind (item 4 — a wired
        /// `DISCONNECT` used to render as "out of range", which is the one
        /// reading that is certainly wrong); one row produces up to **two**
        /// entries; and a row whose onset is outside the range but whose
        /// recovery is inside contributes only the recovery.
        #[tokio::test]
        async fn each_alarm_class_lands_under_its_own_kind_in_the_window() {
            let (state, dir) = state_with_every_kind().await;
            let storage = side_door(&dir);
            let now = crate::render::now_unix();

            for row in [
                // A value alarm that opened and cleared inside the range:
                // two entries from one row.
                alarm("HIGH", now - 3_600, Some(now - 1_800), "too warm"),
                // The portal's own liveness verdict, still open.
                alarm("OFFLINE", now - 7_200, None, "silent"),
                // Onset a day and a half ago (outside), recovery inside:
                // only the recovery belongs in this window.
                alarm("DISCONNECT", now - 130_000, Some(now - 3_000), "unplugged"),
                // ...and one disconnect that opened inside it.
                alarm("DISCONNECT", now - 600, None, "unplugged again"),
            ] {
                storage.insert_alarm_event(&row).expect("insert alarm");
            }

            let html = wired_timeline(&state, 1).await;
            // Counted in the `<ul>` alone: every kind's label also appears
            // in the chart island's JSON config (the marklines read from
            // the same set), so a whole-page count is doubled and would
            // hide exactly the miscount this test is for.
            let list = event_list(&html);

            assert!(list.contains("sensor offline"), "{list}");
            assert!(list.contains("probe disconnected"), "{list}");
            assert!(list.contains("back in range"), "{list}");
            // The whole point of the sixth kind: exactly one row is a value
            // alarm, so "out of range" appears exactly once. Before the fix
            // both `DISCONNECT` rows landed there too.
            assert_eq!(
                list.matches("out of range").count(),
                1,
                "a DISCONNECT is not a value leaving its bounds: {list}"
            );
            // The onset outside the range contributed nothing but its
            // recovery — one "back online", not two.
            assert_eq!(list.matches("back online").count(), 1, "{list}");
        }

        /// UX-021: a value row says how bad, what kind and how far out
        /// (§6.3) instead of a bare "out of range", and its time is
        /// relative with the absolute in the tooltip.
        #[tokio::test]
        async fn a_value_event_names_its_severity_value_and_limit() {
            let (state, dir) = state_with_every_kind().await;
            let now = crate::render::now_unix();
            let mut row = alarm("HIGH", now - 600, None, "");
            row.value = Some(12.1);
            row.threshold = Some(12.0);
            side_door(&dir)
                .insert_alarm_event(&row)
                .expect("insert alarm");

            let html = wired_timeline(&state, 1).await;
            let list = event_list(&html);
            assert!(
                list.contains("critical · High value: 12.1 °C (limit 12.0 °C)"),
                "{list}"
            );
            assert!(list.contains("10 min ago"), "{list}");
        }

        /// A wired line's event timeline, rendered from the window partial.
        /// The Hub's per-probe page carries the Events card (operator notes
        /// only, ui_components.md §5) since 2026-10-15, so the alarm
        /// timeline is checked on the partial that still renders it.
        async fn wired_timeline(state: &AppState, line: u8) -> String {
            let user = crate::auth::CurrentUser {
                id: 1,
                username: "viewer1".into(),
                full_name: None,
                user_role: Role::Viewer,
                is_superuser: false,
                monitoring_view_mode: "table-simplified".into(),
            };
            let q = super::super::WindowQuery {
                range: None,
                error: None,
            };
            let w = super::super::line_window(state, &user, "h1", line, &q)
                .await
                .expect("the line exists");
            body_string(crate::routes::render_fragment(
                state,
                "partials/probe_events.html",
                minijinja::context! { w => w },
            ))
            .await
        }

        /// The rendered `<ul>` of events, without the chart config that
        /// carries the same labels.
        fn event_list(html: &str) -> String {
            let open = html
                .find("<ul class=\"flex flex-col divide-y")
                .expect("the event list is in every window");
            let close = html[open..].find("</ul>").expect("closed") + open;
            html[open..close].to_string()
        }

        /// Review item 7: an outage that opened before the range and never
        /// closed has no edge inside it, so without a line of its own the
        /// window of a probe that has been silent for a week reads exactly
        /// like a quiet week.
        #[tokio::test]
        async fn an_outage_that_spans_the_whole_range_is_still_said_out_loud() {
            let (state, dir) = state_with_every_kind().await;
            let storage = side_door(&dir);
            let now = crate::render::now_unix();

            storage
                .insert_alarm_event(&alarm("OFFLINE", now - 400_000, None, "silent for days"))
                .expect("insert alarm");

            let html = wired_timeline(&state, 1).await;
            assert!(
                html.contains("already offline when this range began"),
                "{html}"
            );
            assert!(
                html.contains("No events in this range."),
                "there is genuinely no edge inside the range: {html}"
            );
        }

        /// Review item 5: `ORDER BY triggered_at DESC` is lexicographic, so
        /// a single `LIMIT` cannot be trusted to have reached the start of
        /// the range — a flapping probe truncated its own history silently.
        /// The walk pages until it is past `from`.
        #[tokio::test]
        async fn the_event_list_pages_past_one_alarm_page() {
            let (state, dir) = state_with_every_kind().await;
            let storage = side_door(&dir);
            let now = crate::render::now_unix();

            // One full page of newer rows, then the one this test is about:
            // the oldest row still inside the 24 h window, which page one
            // cannot contain.
            for i in 0..super::super::ALARM_PAGE_LIMIT {
                storage
                    .insert_alarm_event(&alarm("HIGH", now - 60 - i, Some(now - 30), "flap"))
                    .expect("insert alarm");
            }
            storage
                .insert_alarm_event(&alarm("OFFLINE", now - 80_000, None, "the oldest edge"))
                .expect("insert alarm");

            let html = wired_timeline(&state, 1).await;
            assert!(
                html.contains("the oldest edge"),
                "the walk must reach page two: {html}"
            );
            assert!(
                !html.contains("could not be read"),
                "nothing failed, so nothing should say so: {html}"
            );
        }

        /// `eye_readings` has exactly two numeric columns. Anything else is
        /// a 404, not an empty chart that looks broken.
        #[tokio::test]
        async fn the_beacon_chart_endpoint_serves_temperature_and_humidity_and_refuses_the_rest() {
            let (state, dir) = state_with_every_kind().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            let router = super::super::router().with_state(state.clone());
            let storage = side_door(&dir);

            let now = crate::render::now_unix();
            storage
                .insert_eye_reading(&portal_storage::EyeReading {
                    hostname: HOST.into(),
                    mac: MAC.into(),
                    ts: now - 600,
                    temperature_c: Some(-18.5),
                    humidity_pct: Some(41),
                    event_type: "live".into(),
                    message_id: Some("m-1".into()),
                })
                .expect("insert");

            for (field, expected) in [("temperature", "-18.5"), ("humidity", "41")] {
                let response = get(
                    &router,
                    &format!("/api/v1/chart/eye/{MAC}/{field}"),
                    &viewer,
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK, "{field}");
                let json = body_string(response).await;
                assert!(json.contains(expected), "{field}: {json}");
                assert!(json.contains("\"mac\""), "{field}: {json}");
            }

            for field in ["battery", "movement", "nonsense"] {
                assert_eq!(
                    get(
                        &router,
                        &format!("/api/v1/chart/eye/{MAC}/{field}"),
                        &viewer
                    )
                    .await
                    .status(),
                    StatusCode::NOT_FOUND,
                    "{field}"
                );
            }
            assert_eq!(
                get(
                    &router,
                    "/api/v1/chart/eye/FF:FF:FF:FF:FF:FF/temperature",
                    &viewer
                )
                .await
                .status(),
                StatusCode::NOT_FOUND
            );
        }
    }
}
