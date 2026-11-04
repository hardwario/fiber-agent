//! `/trends` (page; `/history` 308s there), `GET /api/v1/chart/{line}` (JSON), and
//! `GET /export/sensor/{line}.csv` (CSV download) — M3-C.
//!
//! Phase 1 is still single-device (see `AppState`'s doc-comment): every
//! handler here resolves "the device" the same way `render::dashboard_ctx`
//! does — the first entry of the store's `PortalState.devices` `BTreeMap` —
//! rather than taking a hostname from the request.
//!
//! **Granularity policy** (span = `to - from`, in seconds): `<= 2h` -> raw
//! tier, `<= 48h` -> minute tier, else hourly. Pinned by the M3-C work
//! package rather than mirrored from v1 (v1's `/api/temperature/.../history`
//! only has two tiers — raw vs. hourly at a 48h cutoff, plan/v1/viewer/
//! rest.md "Temperature history" — v2 adds the minute tier in between).
//! That policy is what `agg=auto` (the default) still does.
//!
//! **Aggregation** (`agg=auto|raw|30s|1m|10m|1h|1d`, P4): an explicit `agg`
//! names a bucket size instead of letting the span pick a tier. The bucket
//! decides which stored series can serve it — anything under a minute needs
//! `readings_raw`, everything else can be folded out of `readings_minute` —
//! and the rows are then bucketed to mean/min/max in Rust (see
//! [`bucket_points`]). The answer says which bucket it actually used in
//! `agg_effective`, because two things can coarsen it: the stored series is
//! grainier than the request (30s buckets over a window only kept as minute
//! rows), or the bucket would return more than [`MAX_POINTS`] points.
//!
//! **Tier fallback.** `readings_minute` is fed by the Hub's `probe_1m`
//! export stream, which is a *separate* publish from the live probe
//! readings behind `readings_raw` — a Hub paired a minute ago, or one whose
//! export stream is lagging, has raw rows and no minute rows for the same
//! window. Rather than answer "no data" while the data is sitting one table
//! over, a tier that comes back empty is retried against the other one and
//! the response reports what it actually read (`granularity`).
//!
//! **CSV export semantics** mirror v1's `export_sensor` handler
//! (`backend/app/api/export.py`) column-for-column: header
//! `timestamp,device,sensor_line,dev_eui,field,value,temperature,
//! min_celsius,max_celsius`; wired sensors always leave `dev_eui` blank and
//! write `field="temperature"`, `value` == `temperature` == the minute's
//! `avg_c` (v1 exports from its single raw tier; v2's hot tier is
//! `readings_minute`, per this work package's brief). One genuine
//! deviation: v1's `timestamp` column is whatever `datetime.utcnow()
//! .isoformat()` produced when the row was written (a naive, no-timezone,
//! microsecond-precision string) — v2 stores `minute_ts` as unix seconds, so
//! this exports RFC3339 UTC (`…Z` suffix) instead. Same UTC instant, tidier
//! representation; not a byte-identical string.

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use minijinja::context;
use portal_storage::{AlarmListFilter, HourlyBucket, MinuteReading, RawReading};
use serde::{Deserialize, Serialize};

use crate::sensor_picker::SensorPickRow;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::auth::CurrentUser;
use crate::http::AppState;
use crate::render;
use crate::state::SensorThresholds;

use super::render as render_page;

/// `GET /history` → 308 to `/trends`, query string included.
async fn history_redirect(uri: axum::http::Uri) -> Response {
    let target = match uri.query() {
        Some(q) => format!("/trends?{q}"),
        None => "/trends".to_string(),
    };
    (
        StatusCode::PERMANENT_REDIRECT,
        [(axum::http::header::LOCATION, target)],
    )
        .into_response()
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/trends", get(history_page))
        // The page's old address (renamed 2026-10-15, monitoring.md): a
        // bookmark keeps its selection, so the query string goes along.
        .route("/history", get(history_redirect))
        .route("/api/v1/chart/{line}", get(chart_json))
        .route("/export/sensor/{line_csv}", get(export_csv))
        .route("/export/history.csv", get(export_history_csv))
}

// ---------------------------------------------------------------------
// GET /trends
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct HistoryQuery {
    /// Comma-separated selection tokens, see [`Selection`]. The single
    /// source of truth for what the page shows — bookmarkable, and what
    /// the picker's htmx links rewrite.
    s: Option<String>,
    range: Option<String>,
    agg: Option<String>,
    /// `datetime-local` bounds for `range=custom` (parsed by
    /// [`parse_iso8601`], same UTC stance as the CSV export).
    from: Option<String>,
    to: Option<String>,
    /// Pre-P4 single-sensor deep link (`/history?line=3`) — folded into
    /// `s` so old bookmarks and the sensor cards' "history" links still
    /// land on the right sensor.
    line: Option<u8>,
    /// Which Hub's wired lines the probe half of the picker lists — the
    /// `?hub=` rule ([`PortalState::scoped_hub`]). `line:{n}` names no Hub,
    /// so the probe half cannot span them; the Node half always does.
    hub: Option<String>,
    /// `1`: draw the selected channels' events into the chart
    /// (monitoring.md, amendment 2026-10-15). Anything else is off.
    ev: Option<String>,
    /// Comma-separated subset of `alarm,warning,log` (the `/audit` `?type=`
    /// values); absent or empty means all three.
    evt: Option<String>,
}

/// One selected series. The wire form (`line:3`, `node:{key}:{field}`) is
/// what lives in the `?s=` query param; the picker renders one toggle link
/// per candidate rather than posting a form, so the whole selection stays
/// in the URL and survives reload and the back button.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Selection {
    /// A wired probe: its Hub's hostname (lowercase) and line. The token is
    /// `line:{hub}:{n}`; the pre-2026-10-15 `line:{n}` still parses, taking
    /// the Hub from `?hub=` or the only Hub (devices.hub.md, amendment
    /// 2026-10-15).
    Line {
        hub: String,
        line: u8,
    },
    NodeField {
        node_key: String,
        field: String,
    },
    /// A Beacon's temperature or humidity: `eye:{MAC}:{field}` — the MAC
    /// keeps its colons, so the field is whatever follows the last one
    /// (ui_components.md §2, 2026-10-15: the Channels card links here).
    EyeField {
        mac: String,
        field: String,
    },
}

impl Selection {
    fn parse(token: &str, default_hub: Option<&str>) -> Option<Selection> {
        if let Some(rest) = token.strip_prefix("eye:") {
            let (mac, field) = rest.rsplit_once(':')?;
            portal_storage::EyeField::from_field_name(field)?;
            let mac = mac.to_ascii_uppercase();
            return (mac.len() == 17 && mac.split(':').count() == 6).then(|| Selection::EyeField {
                mac,
                field: field.to_string(),
            });
        }
        let mut parts = token.split(':');
        match parts.next()? {
            "line" => {
                let first = parts.next()?;
                let (hub, line) = match parts.next() {
                    Some(n) => (first.to_lowercase(), n),
                    None => (default_hub?.to_lowercase(), first),
                };
                let line = line.parse::<u8>().ok()?;
                (!hub.is_empty() && line <= 7 && parts.next().is_none())
                    .then_some(Selection::Line { hub, line })
            }
            "node" => {
                let node_key = parts.next()?.to_lowercase();
                let field = parts.next()?.to_string();
                (!node_key.is_empty() && !field.is_empty() && parts.next().is_none())
                    .then_some(Selection::NodeField { node_key, field })
            }
            _ => None,
        }
    }

    fn token(&self) -> String {
        match self {
            Selection::Line { hub, line } => format!("line:{hub}:{line}"),
            Selection::NodeField { node_key, field } => format!("node:{node_key}:{field}"),
            Selection::EyeField { mac, field } => format!("eye:{mac}:{field}"),
        }
    }

    fn chart_url(&self) -> String {
        match self {
            Selection::Line { line, .. } => format!("/api/v1/chart/{line}"),
            Selection::NodeField { node_key, field } => {
                format!("/api/v1/chart/node/{node_key}/{field}")
            }
            Selection::EyeField { mac, field } => format!("/api/v1/chart/eye/{mac}/{field}"),
        }
    }
}

/// v1's `MultiSensorChart` palette (`components/dashboard/MultiSensorChart
/// .tsx`'s `COLORS`), copied verbatim so a probe keeps the colour the
/// operator is used to. Also caps the selection: v1's `MAX_SENSORS` is 8,
/// which is exactly one colour each.
pub(crate) const SERIES_COLORS: [&str; 8] = [
    "#10b981", "#3b82f6", "#ef4444", "#f59e0b", "#8b5cf6", "#ec4899", "#06b6d4", "#f97316",
];
const MAX_SENSORS: usize = SERIES_COLORS.len();

/// Splits `?s=`, dropping tokens that don't parse and duplicates, capped at
/// [`MAX_SENSORS`]. Order is the operator's click order, which is what fixes
/// each series' colour.
fn parse_selection(raw: &str, default_hub: Option<&str>) -> Vec<Selection> {
    let mut out: Vec<Selection> = Vec::new();
    for token in raw.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if let Some(sel) = Selection::parse(token, default_hub) {
            if !out.contains(&sel) && out.len() < MAX_SENSORS {
                out.push(sel);
            }
        }
    }
    out
}

/// The page URL for a given selection — every picker row and range/agg pill
/// is a plain link to one of these, so there is no client-side selection
/// state to keep in sync with the address bar.
fn history_href(
    selection: &[Selection],
    range: &str,
    agg: &str,
    from: Option<&str>,
    to: Option<&str>,
    hub: Option<&str>,
) -> String {
    let tokens: Vec<String> = selection.iter().map(Selection::token).collect();
    let mut href = format!("/trends?s={}&range={range}&agg={agg}", tokens.join(","));
    if let (Some(from), Some(to)) = (from, to) {
        href.push_str(&format!("&from={from}&to={to}"));
    }
    // Carried through every toggle and the Clear link, or the page would
    // lose its wired Hub on the first click (devices.hub.md §12 E).
    if let Some(hub) = hub {
        href.push_str(&format!("&hub={hub}"));
    }
    href
}

/// The selection with `sel` added (appended, so it takes the next free
/// colour) or removed.
#[cfg(test)]
fn toggled(selection: &[Selection], sel: &Selection) -> Vec<Selection> {
    if selection.contains(sel) {
        selection.iter().filter(|s| *s != sel).cloned().collect()
    } else if selection.len() < MAX_SENSORS {
        let mut next = selection.to_vec();
        next.push(sel.clone());
        next
    } else {
        selection.to_vec()
    }
}

/// One summary card + chart series for a selected sensor.
#[derive(Serialize)]
struct SelectedCtx {
    name: String,
    hostname: String,
    color: &'static str,
    value: Option<f64>,
    unit: String,
    decimals: usize,
    state: String,
    thresholds: Option<render::ThresholdsCtx>,
    last_seen: Option<String>,
    chart_url: String,
    /// `lrw:{node_key}:` / the bare line number — the `alarm_events`
    /// `sensor_id` prefix this sensor's alarms carry.
    alarm_prefix: String,
    /// The Node channel's field, which narrows `alarm_prefix` to one
    /// channel for the Trends events; `None` for a wired probe.
    alarm_field: Option<String>,
    /// The Node key, for the operator-log half of the Trends events.
    node_key: Option<String>,
}

fn decimals_for(unit: &str) -> usize {
    if unit == "%" {
        0
    } else {
        1
    }
}

/// Picker rows for the wired probes, then one row per chartable Node field
/// that has a value, then each Beacon's temperature / humidity.
/// `wired_hub` is the Hub whose eight wired lines the probe half lists — the
/// `?hub=` rule ([`PortalState::scoped_hub`]), because `line:{n}` names no
/// Hub and so cannot address two of them. The Node half spans every Hub:
/// `node:{key}:{field}` is keyed by a globally unique dev_eui
/// (devices.hub.md §12 E).
fn picker_rows(
    portal_state: &crate::state::PortalState,
    selection: &[Selection],
) -> Vec<SensorPickRow> {
    let row =
        |sel: Selection,
         name: String,
         quantity: String,
         value: Option<f64>,
         unit: String,
         connected: bool,
         (kind, field, location, hub): (&'static str, &str, Option<String>, &str)| {
            let selected = selection.contains(&sel);
            let color = selected
                .then(|| selection.iter().position(|s| *s == sel))
                .flatten()
                .map(|i| SERIES_COLORS[i]);
            let id = sel.token();
            SensorPickRow {
                owner: id.clone(),
                id,
                kind,
                field: field.to_string(),
                name,
                quantity,
                location,
                hub: hub.to_string(),
                value,
                unit,
                connected,
                selected,
                color,
                locked_by: None,
                input_name: None,
            }
        };

    let probes: Vec<SensorPickRow> = portal_state
        .devices
        .values()
        .flat_map(|device| device.sensors.iter().map(move |s| (device, s)))
        .map(|(device, s)| {
            row(
                Selection::Line {
                    hub: device.hostname.to_lowercase(),
                    line: s.line,
                },
                s.name
                    .clone()
                    .unwrap_or_else(|| format!("Probe {}", s.line + 1)),
                crate::i18n::field_label(crate::i18n::current_lang(), "temperature", "Temperature"),
                s.temperature_avg_celsius,
                "\u{b0}C".to_string(),
                s.connected,
                (
                    "probe",
                    "temperature",
                    s.location.clone(),
                    device.hostname.as_str(),
                ),
            )
        })
        .collect();

    let mut internal = Vec::new();
    for (hub, node) in portal_state
        .devices
        .values()
        .flat_map(|d| d.nodes.values().map(move |n| (d.hostname.as_str(), n)))
    {
        let node_name = node.name.clone().unwrap_or_else(|| node.node_key.clone());
        for f in fiber_protocol::lorawan_registry() {
            if !f.chartable || f.kind != fiber_protocol::LorawanFieldKind::Continuous {
                continue;
            }
            let Some(&value) = node.fields.get(f.name) else {
                continue;
            };
            internal.push(row(
                Selection::NodeField {
                    node_key: node.node_key.clone(),
                    field: f.name.to_string(),
                },
                node_name.clone(),
                crate::i18n::field_label(crate::i18n::current_lang(), f.name, f.label),
                Some(f.to_display(value)),
                f.display_unit().unwrap_or("").to_string(),
                true,
                ("node", f.name, node.location.clone(), hub),
            ));
        }
    }

    // Beacons' temperature and humidity (`eye:{MAC}:{field}`, served by
    // `/api/v1/chart/eye/…` since MB3; listed since 2026-10-15, when the
    // Channels card started linking them — ui_components.md §2). Each MAC
    // once, from its owning Hub; a merely heard tag is not a sensor.
    let macs: std::collections::BTreeSet<&String> = portal_state
        .devices
        .values()
        .flat_map(|d| d.ble_sensors.keys())
        .collect();
    for mac in macs {
        let Some((device, b)) = portal_state.hub_of_beacon(mac) else {
            continue;
        };
        if b.discovered == Some(true) {
            continue;
        }
        let name = b.name.clone().unwrap_or_else(|| b.mac.clone());
        for (field, value) in [
            ("temperature", b.temperature),
            ("humidity", b.humidity.map(|h| h as f64)),
        ] {
            let (Some(value), Some(def)) = (value, fiber_protocol::eye_field(field)) else {
                continue;
            };
            internal.push(row(
                Selection::EyeField {
                    mac: b.mac.clone(),
                    field: field.to_string(),
                },
                name.clone(),
                crate::i18n::field_label(crate::i18n::current_lang(), field, def.label),
                Some(value),
                def.unit.unwrap_or("").to_string(),
                true,
                (
                    "beacon",
                    field,
                    b.location.clone(),
                    device.hostname.as_str(),
                ),
            ));
        }
    }

    let mut rows: Vec<SensorPickRow> = probes;
    rows.extend(internal);
    rows
}

/// One sensor group in the picker: a link that replaces the selection with
/// the group's chartable members. Beacon members are left out (nothing
/// serves Beacon history), and wired members only count on the picker's
/// wired Hub, since `line:{n}` names no Hub.
#[derive(Serialize)]
struct GroupPickRow {
    label: String,
    count: usize,
    selected: bool,
    href: String,
    /// The group's selection tokens, comma-joined: the picker's script
    /// sets the table to them (or clears it when the group is active).
    tokens: String,
}

#[allow(clippy::too_many_arguments)]
fn group_rows(
    portal_state: &crate::state::PortalState,
    groups: &[portal_storage::SensorGroup],
    members: &[portal_storage::SensorGroupMember],
    selection: &[Selection],
    range: &str,
    agg: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> Vec<GroupPickRow> {
    groups
        .iter()
        .filter_map(|g| {
            let mut sel: Vec<Selection> = Vec::new();
            for m in members.iter().filter(|m| m.group_id == g.id) {
                if let Some(line) = m.sensor_line {
                    if portal_state.devices.contains_key(&m.hostname) && (0..=7).contains(&line) {
                        sel.push(Selection::Line {
                            hub: m.hostname.to_lowercase(),
                            line: line as u8,
                        });
                    }
                } else if let Some(key) = &m.node_key {
                    let Some((_, node)) = portal_state.hub_of_node(key) else {
                        continue;
                    };
                    for f in fiber_protocol::lorawan_registry() {
                        let wanted = match &m.fields {
                            Some(fields) => fields.iter().any(|x| x == f.name),
                            None => f.name != "battery" && node.fields.contains_key(f.name),
                        };
                        if wanted
                            && f.chartable
                            && f.kind == fiber_protocol::LorawanFieldKind::Continuous
                        {
                            sel.push(Selection::NodeField {
                                node_key: node.node_key.clone(),
                                field: f.name.to_string(),
                            });
                        }
                    }
                }
            }
            sel.dedup();
            sel.truncate(MAX_SENSORS);
            (!sel.is_empty()).then(|| GroupPickRow {
                label: g.name.clone(),
                count: sel.len(),
                selected: sel.as_slice() == selection,
                href: history_href(&sel, range, agg, from, to, None),
                tokens: sel
                    .iter()
                    .map(Selection::token)
                    .collect::<Vec<_>>()
                    .join(","),
            })
        })
        .collect()
}

/// The summary card / chart series context for each selected sensor, in
/// selection order (which is colour order). `node_thresholds` is the
/// already-merged per-field view (`render::merge_node_thresholds`) for the
/// Nodes in the selection — passed in because reading it needs the DB and
/// this stays a pure mapping.
fn selected_ctx(
    portal_state: &crate::state::PortalState,
    selection: &[Selection],
    node_thresholds: &std::collections::HashMap<String, render::ThresholdsCtx>,
) -> Vec<SelectedCtx> {
    selection
        .iter()
        .enumerate()
        .filter_map(|(i, sel)| {
            let color = SERIES_COLORS[i];
            match sel {
                Selection::Line { hub, line } => {
                    // The token names its Hub; a Hub no longer in the store
                    // drops the series.
                    let device = portal_state
                        .devices
                        .values()
                        .find(|d| d.hostname.eq_ignore_ascii_case(hub))?;
                    let s = device.sensors.get(*line as usize)?;
                    // Everything but the timestamp comes from the shared
                    // card mapper; `updated_at` there is HH:MM:SS, and this
                    // card wants the date too (the window can be a year).
                    let card = render::sensor_card_ctx(&device.hostname, s, false);
                    Some(SelectedCtx {
                        name: card
                            .name
                            .clone()
                            .unwrap_or_else(|| format!("Probe {}", line + 1)),
                        hostname: device.hostname.clone(),
                        color,
                        value: card.avg_c,
                        unit: "\u{b0}C".to_string(),
                        decimals: 1,
                        state: card.dominant_alarm_state.clone(),
                        thresholds: card.thresholds.clone(),
                        last_seen: s.updated_at.map(format_rfc3339),
                        chart_url: sel.chart_url(),
                        alarm_prefix: format!("line-{line}"),
                        alarm_field: None,
                        node_key: None,
                    })
                }
                Selection::NodeField { node_key, field } => {
                    // The owning Hub, wherever it is in the store.
                    let (device, node) = portal_state.hub_of_node(node_key)?;
                    let f = fiber_protocol::lorawan_field(field)?;
                    let unit = f.display_unit().unwrap_or("").to_string();
                    let node_name = node.name.clone().unwrap_or_else(|| node_key.clone());
                    Some(SelectedCtx {
                        name: format!(
                            "{node_name} ({})",
                            crate::i18n::field_label(crate::i18n::current_lang(), f.name, f.label)
                        ),
                        hostname: device.hostname.clone(),
                        color,
                        value: node.fields.get(field).map(|&v| f.to_display(v)),
                        decimals: decimals_for(&unit),
                        unit,
                        state: node
                            .field_alarm_states
                            .get(field)
                            .map(|s| render::normalize_radio_alarm_state(s))
                            .unwrap_or_else(|| "Normal".to_string()),
                        thresholds: node_thresholds.get(&format!("{node_key}:{field}")).cloned(),
                        last_seen: node.last_seen.clone(),
                        chart_url: sel.chart_url(),
                        alarm_prefix: portal_domain::node_sensor_prefix(node_key),
                        alarm_field: Some(field.clone()),
                        node_key: Some(node_key.clone()),
                    })
                }
                Selection::EyeField { mac, field } => {
                    let (device, beacon) = portal_state.hub_of_beacon(mac)?;
                    let def = fiber_protocol::eye_field(field)?;
                    let unit = def.unit.unwrap_or("").to_string();
                    let name = beacon.name.clone().unwrap_or_else(|| mac.clone());
                    let value = match field.as_str() {
                        "temperature" => beacon.temperature,
                        _ => beacon.humidity.map(|h| h as f64),
                    };
                    Some(SelectedCtx {
                        name: format!(
                            "{name} ({})",
                            crate::i18n::field_label(
                                crate::i18n::current_lang(),
                                def.name,
                                def.label
                            )
                        ),
                        hostname: device.hostname.clone(),
                        color,
                        value,
                        decimals: decimals_for(&unit),
                        unit,
                        state: "Normal".to_string(),
                        thresholds: None,
                        last_seen: None,
                        chart_url: sel.chart_url(),
                        alarm_prefix: format!("eye:{mac}:"),
                        alarm_field: Some(field.clone()),
                        node_key: None,
                    })
                }
            }
        })
        .collect()
}

/// Alarms for the selected sensors inside the charted window, newest first
/// — the same "one list across every selected sensor" section v1's compare
/// view has under its chart. Matched on `alarm_engine`'s own `sensor_id`
/// convention: exactly `line-{n}` for a wired probe, anything under the
/// `lrw:{node_key}:` prefix for a Node (one alarm row per field, and this
/// section lists them all, as v1 does).
/// The selection may span Hubs, so a row must match on hostname as well as
/// `sensor_id`: `line-0` names a different probe on every Hub. The DB-side
/// hostname filter is kept for the single-Hub selection, which is what it
/// was there for; a mixed selection reads the window and matches in memory
/// (devices.hub.md §12 E).
// No longer rendered by Trends (2026-10-15): the alarm history moved to
// /audit, which may reuse this query.
#[allow(dead_code)]
async fn recent_alarms_ctx(
    state: &AppState,
    selected: &[SelectedCtx],
    from: i64,
    to: i64,
) -> Vec<HistoryAlarmCtx> {
    if selected.is_empty() {
        return Vec::new();
    }
    let snapshot = state.store.snapshot();
    let hosts: std::collections::BTreeSet<&str> =
        selected.iter().map(|s| s.hostname.as_str()).collect();
    let filter = AlarmListFilter {
        limit: 500,
        hostname: (hosts.len() == 1)
            .then(|| hosts.iter().next().map(|h| h.to_string()))
            .flatten(),
        ..Default::default()
    };
    let Ok((rows, _)) = state.alarms.list(filter).await else {
        return Vec::new();
    };
    rows.iter()
        .filter(|a| parse_iso8601(&a.triggered_at).is_ok_and(|ts| ts >= from && ts <= to))
        .filter_map(|a| {
            let owner = selected.iter().find(|s| {
                if s.hostname != a.hostname {
                    return false;
                }
                if s.alarm_prefix.ends_with(':') {
                    a.sensor_id.starts_with(&s.alarm_prefix)
                } else {
                    a.sensor_id == s.alarm_prefix
                }
            })?;
            Some(HistoryAlarmCtx {
                alarm: render::alarm_ctx(a, &snapshot),
                color: owner.color,
                sensor_label: owner.name.clone(),
            })
        })
        .take(20)
        .collect()
}

/// One row of the History page's "Recent alarms" list: the shared alarm
/// context plus which selected series it belongs to, so the row can carry
/// that series' colour and name instead of the raw `sensor_id`.
#[derive(Serialize)]
struct HistoryAlarmCtx {
    #[serde(flatten)]
    alarm: render::AlarmCtx,
    color: &'static str,
    sensor_label: String,
}

/// Merged Node field thresholds for the Nodes in the selection, keyed
/// `"{node_key}:{field}"`. Same precedence as the Node detail page (state
/// bounds win over the DB's, see `render::merge_node_thresholds`) — the
/// History summary card must not disagree with the page the operator sets
/// them on.
async fn node_thresholds_ctx(
    state: &AppState,
    portal_state: &crate::state::PortalState,
    selection: &[Selection],
) -> std::collections::HashMap<String, render::ThresholdsCtx> {
    let mut out = std::collections::HashMap::new();
    for sel in selection {
        let Selection::NodeField { node_key, field } = sel else {
            continue;
        };
        // The stored rows are keyed by the OWNING Hub's hostname, so the
        // lookup has to find it rather than assume one (devices.hub.md §12 E).
        let Some((device, node)) = portal_state.hub_of_node(node_key) else {
            continue;
        };
        let db = ({
            let a0 = device.hostname.clone();
            let a1 = portal_storage::THRESHOLD_SCOPE_NODE.to_string();
            let a2 = node_key.clone();
            state
                .db
                .run(move |s| s.list_field_thresholds(&a0, &a1, &a2))
        })
        .await
        .unwrap_or_default();
        let merged = render::displayed_node_thresholds(node.field_thresholds.as_deref(), &db);
        if let Some(t) = merged.get(field) {
            out.insert(
                format!("{node_key}:{field}"),
                render::ThresholdsCtx {
                    critical_low: t.critical_low,
                    warning_low: t.warning_low,
                    warning_high: t.warning_high,
                    critical_high: t.critical_high,
                    alarm_low: None,
                    alarm_high: None,
                    hysteresis: None,
                    enabled: true,
                },
            );
        }
    }
    out
}

async fn history_page(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Query(q): Query<HistoryQuery>,
) -> Response {
    let portal_state = state.store.snapshot();
    // The picker's Node half spans every Hub; its wired half belongs to one,
    // picked by `?hub=` or by there being only one (devices.hub.md §12 E).
    let wired_hub = portal_state.scoped_hub(q.hub.as_deref());

    let range = q.range.clone().unwrap_or_else(|| "24h".to_string());
    let range = if RANGES.contains(&range.as_str()) || range == "custom" {
        range
    } else {
        "24h".to_string()
    };
    // No `?agg=` means 10-minute buckets (monitoring.md, amendment
    // 2026-10-15); `auto` stays one option of the select.
    let agg = Agg::parse(Some(q.agg.as_deref().unwrap_or(DEFAULT_TRENDS_AGG)));
    let (from_raw, to_raw) = match (q.from.as_deref(), q.to.as_deref()) {
        (Some(f), Some(t)) if range == "custom" => (Some(f), Some(t)),
        _ => (None, None),
    };

    let selection = match (&q.s, q.line) {
        // A legacy `line:{n}` token takes its Hub from `?hub=` or the only
        // Hub; every link the page renders then names it (`line:{hub}:{n}`).
        (Some(s), _) => parse_selection(s, wired_hub.map(|d| d.hostname.as_str())),
        (None, Some(line)) if line <= 7 => wired_hub
            .map(|d| Selection::Line {
                hub: d.hostname.to_lowercase(),
                line,
            })
            .into_iter()
            .collect(),
        _ => Vec::new(),
    };

    if portal_state.devices.is_empty() {
        let ctx = render::page_ctx(
            context! {
                nav_active => "history",
                has_device => false,
                hostname => None::<String>,
            },
            Some(&current_user),
        );
        return render_page(&state, "pages/history.html", ctx);
    }

    let rows = picker_rows(&portal_state, &selection);
    // The search haystack's descriptions (B1), by row token.
    let descs = crate::monitoring::Descriptions::load(&state.db, &portal_state)
        .await
        .by_id(rows.iter().map(|r| r.id.as_str()));
    // The Quantity filter's options: the fields the table lists, labelled as its rows are.
    let mut fields: Vec<(String, String)> = Vec::new();
    for r in &rows {
        if !fields.iter().any(|(f, _)| *f == r.field) {
            fields.push((r.field.clone(), r.quantity.clone()));
        }
    }
    let (groups, members) = state
        .db
        .run(|s| Ok((s.list_sensor_groups()?, s.list_all_group_members()?)))
        .await
        .unwrap_or_default();
    let groups = group_rows(
        &portal_state,
        &groups,
        &members,
        &selection,
        &range,
        agg.as_str(),
        from_raw,
        to_raw,
    );
    let node_thresholds = node_thresholds_ctx(&state, &portal_state, &selection).await;
    let selected = selected_ctx(&portal_state, &selection, &node_thresholds);
    let ev_opts = EventOpts::parse(q.ev.as_deref(), q.evt.as_deref());
    let ev_qs = ev_opts.qs();
    let can_read_log = current_user.has_role(crate::auth::Role::Manager);
    let ev_shown = ev_opts.shown(can_read_log);
    let mut groups = groups;
    for g in &mut groups {
        g.href.push_str(&ev_qs);
    }

    let now = now_unix();
    let (from_ts, to_ts) = resolve_range_parts(
        Some(&range),
        from_raw.and_then(crate::display_tz::parse_local),
        to_raw.and_then(crate::display_tz::parse_local),
        now,
    )
    .unwrap_or((now - 24 * 3600, now));

    let events = if ev_opts.on {
        trend_events(
            &state,
            &selected,
            from_ts,
            to_ts,
            ev_opts.effective(can_read_log),
        )
        .await
    } else {
        Vec::new()
    };

    // One JSON blob for static/js/portal-charts.js — see that file's header.
    // Serialized here rather than in the template: minijinja is built
    // without its `json` feature, so there is no `tojson` filter.
    let chart_config = serde_json::json!({
        "range": range,
        "agg": agg.as_str(),
        "from": from_ts,
        "to": to_ts,
        "thresholds": selected.len() == 1,
        "events": events,
        "series": selected.iter().map(|s| serde_json::json!({
            "url": s.chart_url,
            "name": s.name,
            "color": s.color,
            "unit": s.unit,
            // The Hub this series belongs to, appended to the fetch as
            // `?hub=`. The wired endpoint needs it once the store holds more
            // than one Hub (devices.hub.md §12 E); the Node and Beacon
            // endpoints resolve the Hub from the key and ignore it.
            "hub": s.hostname,
        })).collect::<Vec<_>>(),
    });
    // `</` cannot appear literally inside a <script> element, even a JSON
    // one — a sensor named "</script>" would otherwise end the block early.
    let chart_config = chart_config.to_string().replace("</", "<\\/");

    let tokens: Vec<String> = selection.iter().map(Selection::token).collect();
    let ctx = render::page_ctx(
        context! {
            nav_active => "history",
            // The store holds at least one Hub (the empty store returned
            // above). The page gates on this, not on `hostname`: a Node
            // series needs no wired Hub.
            has_device => true,
            // The wired half's Hub, which is what the device row names;
            // `None` when several Hubs are in the store and the request
            // carried no `?hub=`, and then the probe list is empty too.
            hostname => wired_hub.map(|d| d.hostname.clone()),
            // v1's picker heads the device row with the operator-assigned
            // label and keeps the hostname for the summary cards.
            device_label => wired_hub.map(|d| d
                .system
                .device_label
                .clone()
                .unwrap_or_else(|| d.hostname.clone())),
            device_online => wired_hub.is_some_and(|d| d.online),
            descs => descs,
            rows => rows,
            groups => groups,
            selected => selected,
            selected_tokens => tokens.join(","),
            // The page script swaps `s` in this URL for the table's
            // selection (portal-select.js), so every other parameter stays.
            select_base => history_href(
                &[],
                &range,
                agg.as_str(),
                from_raw,
                to_raw,
                None,
            ) + &ev_qs,
            many_hubs => portal_state.devices.len() > 1,
            fields => fields,
            // Token → colour of the current series, for the table's swatches,
            // which sit outside the swapped region (pages/history.html).
            series_colors => selection
                .iter()
                .enumerate()
                .map(|(i, s)| (s.token(), SERIES_COLORS[i]))
                .collect::<std::collections::BTreeMap<_, _>>(),
            selected_count => selection.len(),
            max_sensors => MAX_SENSORS,
            ranges => RANGES,
            aggs => AGG_PILLS,
            selected_range => range,
            selected_agg => agg.as_str(),
            // Only a bucket the URL names is the operator's choice; the
            // default lets the chart show the bucket actually served.
            agg_explicit => q.agg.is_some() && agg != Agg::Auto,
            custom_from => from_raw,
            custom_to => to_raw,
            chart_config => chart_config,
            ev_on => ev_opts.on,
            ev_qs => ev_qs,
            ev_evt => ev_qs.split("&evt=").nth(1).map(str::to_string),
            // The Events dropdown (amendment 2026-10-15): checked = shown,
            // none checked = events off. Each box links to this URL with its
            // kind flipped, everything else kept.
            ev_count => ev_shown.iter().filter(|k| **k).count(),
            ev_kinds => EVENT_KINDS.iter().enumerate()
                // The log is manager-and-above (decision #18).
                .filter(|(_, k)| **k != "log" || can_read_log)
                .map(|(i, k)| {
                    let mut kinds = ev_shown;
                    kinds[i] = !kinds[i];
                    let next = EventOpts::from_shown(kinds);
                    context! {
                        kind => *k,
                        checked => ev_shown[i],
                        href => history_href(&selection, &range, agg.as_str(), from_raw,
                            to_raw, None) + &next.qs(),
                    }
                })
                .collect::<Vec<_>>(),
            clear_href => history_href(
                &[],
                &range,
                agg.as_str(),
                from_raw,
                to_raw,
                None,
            ) + &ev_qs,
        },
        Some(&current_user),
    );
    render_page(&state, "pages/history.html", ctx)
}

// ---------------------------------------------------------------------
// Trends events (monitoring.md, amendment 2026-10-15)
// ---------------------------------------------------------------------

/// The `/audit` `?type=` values, in the order the toolbar lists them.
const EVENT_KINDS: [&str; 3] = ["alarm", "warning", "log"];
/// Cap on the drawn lines; the newest win.
const MAX_TREND_EVENTS: usize = 200;

/// The `ev` / `evt` query pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EventOpts {
    on: bool,
    /// Indexed like [`EVENT_KINDS`].
    kinds: [bool; 3],
}

impl EventOpts {
    fn parse(ev: Option<&str>, evt: Option<&str>) -> Self {
        let mut kinds = [false; 3];
        for t in evt.unwrap_or("").split(',') {
            if let Some(i) = EVENT_KINDS.iter().position(|k| *k == t.trim()) {
                kinds[i] = true;
            }
        }
        if !kinds.iter().any(|k| *k) {
            kinds = [true; 3];
        }
        Self {
            on: ev == Some("1"),
            kinds,
        }
    }

    /// The query suffix carried by every Trends link (`""` when off, and
    /// `evt` only when it narrows).
    fn qs(&self) -> String {
        if !self.on {
            return String::new();
        }
        let mut out = "&ev=1".to_string();
        if !self.kinds.iter().all(|k| *k) && self.kinds.iter().any(|k| *k) {
            let picked: Vec<&str> = EVENT_KINDS
                .iter()
                .zip(self.kinds)
                .filter_map(|(k, on)| on.then_some(*k))
                .collect();
            out.push_str(&format!("&evt={}", picked.join(",")));
        }
        out
    }

    /// The kinds drawn, as the dropdown's boxes: none while off.
    fn shown(&self, can_read_log: bool) -> [bool; 3] {
        if self.on {
            self.effective(can_read_log)
        } else {
            [false; 3]
        }
    }

    /// The options for a set of shown kinds: off when none is.
    fn from_shown(kinds: [bool; 3]) -> Self {
        let on = kinds.iter().any(|k| *k);
        Self {
            on,
            kinds: if on { kinds } else { [true; 3] },
        }
    }

    /// The kinds actually read: the operator log only for manager and above,
    /// as on `/audit` (decision #18).
    fn effective(&self, can_read_log: bool) -> [bool; 3] {
        let mut k = self.kinds;
        k[2] &= can_read_log;
        k
    }
}

/// One vertical line of the Trends chart (`portal-charts.js` `events`).
#[derive(Debug, Serialize, PartialEq)]
struct TrendEvent {
    /// Unix seconds.
    ts: i64,
    /// `alarm` | `warning` | `log`.
    kind: &'static str,
    /// The item the event is about: the series name, or the log's target.
    item: String,
    message: String,
    /// The series colour the event belongs to, for the tooltip swatch.
    color: &'static str,
    href: String,
}

/// Whether an alarm row belongs to one selected channel: the exact
/// `line-{n}` on its Hub for a wired probe, `lrw:{node_key}:{label}` (or
/// the Node's `liveness`) for a Node channel.
fn alarm_on_channel(s: &SelectedCtx, hostname: &str, sensor_id: &str) -> bool {
    if s.hostname != hostname {
        return false;
    }
    match &s.alarm_field {
        // The id ends in the field's label (`:Temperature`), matched without
        // case as the alarm list's own `sensor_field` filter does.
        Some(field) => sensor_id
            .strip_prefix(s.alarm_prefix.as_str())
            .is_some_and(|rest| {
                rest.eq_ignore_ascii_case(field)
                    || rest.eq_ignore_ascii_case("liveness")
                    || fiber_protocol::lorawan_field(field)
                        .is_some_and(|f| rest.eq_ignore_ascii_case(f.label))
            }),
        None => sensor_id == s.alarm_prefix,
    }
}

/// The `audit_log.ts` shape (`YYYY-MM-DD HH:MM:SS`, UTC) of a unix time.
fn audit_ts(unix: i64) -> String {
    let t = format_rfc3339(unix);
    t.get(..19).unwrap_or(&t).replace('T', " ")
}

/// The selected channels' events inside `from..=to`, read from the same two
/// sources `/audit` merges: the alarm list (one read per wanted severity,
/// bounded by `triggered_*`) and the operator log (entries targeting a
/// selected Node). `kinds` is [`EventOpts::effective`].
async fn trend_events(
    state: &AppState,
    selected: &[SelectedCtx],
    from: i64,
    to: i64,
    kinds: [bool; 3],
) -> Vec<TrendEvent> {
    if selected.is_empty() {
        return Vec::new();
    }
    let since = audit_ts(from);
    let until = audit_ts(to);
    let hosts: std::collections::BTreeSet<&str> =
        selected.iter().map(|s| s.hostname.as_str()).collect();
    let mut out = Vec::new();
    for (i, sev) in [(0usize, "critical"), (1, "warning")] {
        if !kinds[i] {
            continue;
        }
        let filter = AlarmListFilter {
            limit: 1000,
            include_acknowledged: true,
            include_cleared: true,
            severity: Some(sev.into()),
            hostname: (hosts.len() == 1)
                .then(|| hosts.iter().next().map(|h| h.to_string()))
                .flatten(),
            triggered_since: Some(since.clone()),
            triggered_until: Some(until.clone()),
            ..Default::default()
        };
        let rows = match state.alarms.list(filter).await {
            Ok((rows, _)) => rows,
            Err(err) => {
                tracing::warn!(%err, "failed to read alarms for the trends events");
                continue;
            }
        };
        for a in rows {
            let Some(owner) = selected
                .iter()
                .find(|s| alarm_on_channel(s, &a.hostname, &a.sensor_id))
            else {
                continue;
            };
            let Ok(ts) = parse_iso8601(&a.triggered_at) else {
                continue;
            };
            out.push(TrendEvent {
                ts,
                kind: EVENT_KINDS[i],
                item: owner.name.clone(),
                message: a.message.clone().unwrap_or_else(|| a.alarm_type.clone()),
                color: owner.color,
                href: format!("/alarms/{}", a.id),
            });
        }
    }
    if kinds[2] {
        let mut keys: Vec<(&str, &SelectedCtx)> = Vec::new();
        for s in selected {
            if let Some(key) = s.node_key.as_deref() {
                if !keys.iter().any(|(k, _)| *k == key) {
                    keys.push((key, s));
                }
            }
        }
        for (key, owner) in keys {
            let filter = portal_storage::AuditFilter {
                since: Some(since.clone()),
                until: Some(until.clone()),
                search: Some(key.to_string()),
                limit: 500,
                ..Default::default()
            };
            let entries = match state.auth.audit_page(filter).await {
                Ok((entries, _)) => entries,
                Err(err) => {
                    tracing::warn!(%err, "failed to read the audit log for the trends events");
                    continue;
                }
            };
            out.extend(log_events(&entries, key, owner));
        }
    }
    out.sort_by(|a, b| b.ts.cmp(&a.ts));
    out.truncate(MAX_TREND_EVENTS);
    out
}

/// The log entries that target `node_key` (the search also matches the
/// key inside other columns), as chart events.
fn log_events(
    entries: &[portal_storage::AuditEntry],
    node_key: &str,
    owner: &SelectedCtx,
) -> Vec<TrendEvent> {
    entries
        .iter()
        .filter(|e| {
            e.target
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(node_key))
        })
        .filter_map(|e| {
            let ts = parse_iso8601(&e.ts.replacen(' ', "T", 1)).ok()?;
            let day = e.ts.get(..10).unwrap_or("");
            Some(TrendEvent {
                ts,
                kind: "log",
                item: owner
                    .name
                    .split(" (")
                    .next()
                    .unwrap_or(&owner.name)
                    .to_string(),
                message: match &e.username {
                    Some(u) => format!("{} ({u})", e.event_type),
                    None => e.event_type.clone(),
                },
                color: owner.color,
                href: format!("/audit?type=log&q={node_key}&since={day}&until={day}"),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------
// GET /api/v1/chart/{line}
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Granularity {
    Raw,
    Minute,
    Hour,
}

impl Granularity {
    fn as_str(self) -> &'static str {
        match self {
            Granularity::Raw => "raw",
            Granularity::Minute => "minute",
            Granularity::Hour => "hour",
        }
    }

    /// How coarse the stored rows of this tier are — the floor on any
    /// bucket built out of them.
    fn grain_secs(self) -> i64 {
        match self {
            Granularity::Raw => 1,
            Granularity::Minute => 60,
            Granularity::Hour => 3600,
        }
    }
}

const TWO_HOURS_SECS: i64 = 2 * 3600;
const FORTY_EIGHT_HOURS_SECS: i64 = 48 * 3600;

/// The granularity policy documented in this module's header comment, as a
/// pure function of the requested span in seconds.
fn granularity_for_span(span_secs: i64) -> Granularity {
    if span_secs <= TWO_HOURS_SECS {
        Granularity::Raw
    } else if span_secs <= FORTY_EIGHT_HOURS_SECS {
        Granularity::Minute
    } else {
        Granularity::Hour
    }
}

/// The time-window pills, v1's `PRESET_ORDER`.
const RANGES: [&str; 6] = ["1h", "6h", "24h", "7d", "30d", "1y"];

/// The aggregation pills, v1's `Interval` order. `auto` is not one of them:
/// it is what the page defaults to before the operator picks a bucket, and
/// it reports back through `agg_effective` which pill to light up.
const AGG_PILLS: [&str; 7] = ["auto", "raw", "30s", "1m", "10m", "1h", "1d"];
/// The Trends page's bucket when the URL names none.
const DEFAULT_TRENDS_AGG: &str = "10m";

/// The requested aggregation bucket. Parsed leniently (an unknown value is
/// `Auto`) because it arrives from a query string; the response always says
/// what was actually used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Agg {
    Auto,
    Raw,
    S30,
    M1,
    M10,
    H1,
    D1,
}

impl Agg {
    pub(crate) fn parse(raw: Option<&str>) -> Agg {
        match raw.unwrap_or("auto") {
            "raw" => Agg::Raw,
            "30s" => Agg::S30,
            "1m" => Agg::M1,
            "10m" => Agg::M10,
            "1h" => Agg::H1,
            "1d" => Agg::D1,
            _ => Agg::Auto,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Agg::Auto => "auto",
            Agg::Raw => "raw",
            Agg::S30 => "30s",
            Agg::M1 => "1m",
            Agg::M10 => "10m",
            Agg::H1 => "1h",
            Agg::D1 => "1d",
        }
    }

    /// Bucket width in seconds. `Auto` and `Raw` are 0 — "don't re-bucket,
    /// hand back the stored rows".
    fn bucket_secs(self) -> i64 {
        match self {
            Agg::Auto | Agg::Raw => 0,
            Agg::S30 => 30,
            Agg::M1 => 60,
            Agg::M10 => 600,
            Agg::H1 => 3600,
            Agg::D1 => 86_400,
        }
    }

    /// Smallest pill whose bucket is at least `secs` — used to name the
    /// bucket the server escalated to when the request was too fine.
    fn from_bucket_secs(secs: i64) -> Agg {
        for agg in [Agg::S30, Agg::M1, Agg::M10, Agg::H1] {
            if secs <= agg.bucket_secs() {
                return agg;
            }
        }
        Agg::D1
    }
}

/// Ceiling on how many points one series may carry back. A chart 900 px
/// wide cannot show more, and `agg=raw` over a week would otherwise ship
/// ~300 000 of them. Only an explicit `agg` can trip it — `auto`'s tiers
/// are bounded by construction, and staying out of its way is what keeps
/// the node detail page's JSON byte-identical to before.
const MAX_POINTS: usize = 4000;

/// Folds an ascending-by-`ts` series into fixed `bucket_secs` slots: each
/// slot's `ts` is its floor-aligned start, `avg` the arithmetic mean of the
/// points' `avg`, `min`/`max` the extremes of the points' `min`/`max`.
/// Slots with no point are simply absent, so a gap in the data stays a gap
/// in the chart rather than becoming a zero. `bucket_secs <= 0` is the
/// identity.
pub(crate) fn bucket_points(points: Vec<ChartPoint>, bucket_secs: i64) -> Vec<ChartPoint> {
    if bucket_secs <= 0 {
        return points;
    }
    let mut out: Vec<ChartPoint> = Vec::new();
    let mut sum = 0.0f64;
    let mut count = 0usize;

    for p in points {
        let slot = p.ts.div_euclid(bucket_secs) * bucket_secs;
        match out.last_mut() {
            Some(last) if last.ts == slot => {
                last.min = min_opt(last.min, p.min);
                last.max = max_opt(last.max, p.max);
            }
            _ => {
                if let Some(last) = out.last_mut() {
                    last.avg = (count > 0).then(|| sum / count as f64);
                }
                sum = 0.0;
                count = 0;
                out.push(ChartPoint {
                    ts: slot,
                    avg: None,
                    min: p.min,
                    max: p.max,
                });
            }
        }
        if let Some(avg) = p.avg {
            sum += avg;
            count += 1;
        }
    }
    if let Some(last) = out.last_mut() {
        last.avg = (count > 0).then(|| sum / count as f64);
    }
    out
}

fn min_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn max_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Applies `agg` to a stored series: buckets it, then escalates the bucket
/// until the result fits [`MAX_POINTS`]. Returns the points plus the pill
/// name the UI should light up.
///
/// `source_grain` is how coarse the stored rows already are (1 s for raw,
/// 60 s for minute, 3600 s for hourly) — asking for 30 s buckets over
/// minute rows cannot invent the missing detail, so the answer says `1m`.
pub(crate) fn apply_agg(
    points: Vec<ChartPoint>,
    agg: Agg,
    source_grain: i64,
    span_secs: i64,
) -> (Vec<ChartPoint>, &'static str) {
    if agg == Agg::Auto {
        let effective = match source_grain {
            g if g <= 1 => Agg::Raw,
            g if g <= 60 => Agg::M1,
            _ => Agg::H1,
        };
        if points.len() <= MAX_POINTS {
            return (points, effective.as_str());
        }
        // The span-driven tiers are bounded well under the cap, so this
        // only fires when the tier fallback handed back raw rows for a
        // window that was meant to be served coarser — bucket them to the
        // density the tier would have had rather than ship a megabyte.
        let bucket = Agg::from_bucket_secs(span_secs.div_euclid(MAX_POINTS as i64).max(1));
        return (bucket_points(points, bucket.bucket_secs()), bucket.as_str());
    }

    let mut bucket = agg.bucket_secs().max(source_grain);
    if agg == Agg::Raw && source_grain <= 1 {
        bucket = 0;
    }
    // One escalation is enough: the bucket that makes the span fit is
    // computed directly, not searched for.
    let want = if bucket > 0 {
        span_secs.div_euclid(bucket) as usize
    } else {
        points.len()
    };
    if want > MAX_POINTS || points.len() > MAX_POINTS {
        let needed = span_secs.div_euclid(MAX_POINTS as i64).max(1);
        bucket = bucket.max(Agg::from_bucket_secs(needed).bucket_secs());
    }

    let effective = if bucket == 0 {
        Agg::Raw
    } else {
        Agg::from_bucket_secs(bucket)
    };
    (bucket_points(points, bucket), effective.as_str())
}

#[derive(Deserialize)]
struct ChartQuery {
    range: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    agg: Option<String>,
    /// Which Hub's line `{line}` is — the `?hub=` rule
    /// ([`PortalState::scoped_hub`]). A wired line is Hub-scoped and the path
    /// carries no hostname, so with several Hubs this parameter is the only
    /// thing that says whose history to read (devices.hub.md §12 E).
    hub: Option<String>,
}

/// Resolves `(from, to)` unix-seconds bounds: explicit `from`+`to` win when
/// both are present (validated `to > from`); otherwise a named `range`
/// (`1h`/`6h`/`24h`/`7d`/`30d`, defaulting to `24h` for an unset or
/// unrecognized value) is resolved against `now`.
///
/// `pub(crate)`: `routes::node_routes`'s chart endpoint takes the identical
/// `range`/`from`/`to` params ("like the wired endpoint", M5-G) — shared
/// here rather than re-implemented against a second `ChartQuery`-shaped
/// struct.
pub(crate) fn resolve_range_parts(
    range: Option<&str>,
    from: Option<i64>,
    to: Option<i64>,
    now: i64,
) -> Result<(i64, i64), &'static str> {
    if let (Some(from), Some(to)) = (from, to) {
        if to <= from {
            return Err("'to' must be after 'from'");
        }
        return Ok((from, to));
    }

    let secs = match range.unwrap_or("24h") {
        "1h" => 3600,
        "6h" => 6 * 3600,
        "24h" => 24 * 3600,
        "7d" => 7 * 24 * 3600,
        "30d" => 30 * 24 * 3600,
        "1y" => 365 * 24 * 3600,
        _ => 24 * 3600,
    };
    Ok((now - secs, now))
}

fn resolve_range(q: &ChartQuery, now: i64) -> Result<(i64, i64), &'static str> {
    resolve_range_parts(q.range.as_deref(), q.from, q.to, now)
}

#[derive(Clone, Serialize)]
pub(crate) struct ChartPoint {
    pub(crate) ts: i64,
    pub(crate) avg: Option<f64>,
    pub(crate) min: Option<f64>,
    pub(crate) max: Option<f64>,
}

impl From<&RawReading> for ChartPoint {
    fn from(r: &RawReading) -> Self {
        ChartPoint {
            ts: r.ts,
            avg: r.avg_c,
            min: r.min_c,
            max: r.max_c,
        }
    }
}

impl From<&MinuteReading> for ChartPoint {
    fn from(r: &MinuteReading) -> Self {
        ChartPoint {
            ts: r.minute_ts,
            avg: r.avg_c,
            min: r.min_c,
            max: r.max_c,
        }
    }
}

impl From<&HourlyBucket> for ChartPoint {
    fn from(b: &HourlyBucket) -> Self {
        ChartPoint {
            ts: b.hour_ts,
            avg: b.avg_c,
            min: b.min_c,
            max: b.max_c,
        }
    }
}

/// `GET /api/v1/chart/{line}`'s `thresholds` object — field names match
/// `state::SensorThresholds` exactly (see that struct's doc-comment).
#[derive(Serialize)]
struct ThresholdsJson {
    critical_low: Option<f64>,
    warning_low: Option<f64>,
    warning_high: Option<f64>,
    critical_high: Option<f64>,
    alarm_low: Option<f64>,
    alarm_high: Option<f64>,
}

impl From<&SensorThresholds> for ThresholdsJson {
    fn from(t: &SensorThresholds) -> Self {
        ThresholdsJson {
            critical_low: t.critical_low,
            warning_low: t.warning_low,
            warning_high: t.warning_high,
            critical_high: t.critical_high,
            alarm_low: t.alarm_low,
            alarm_high: t.alarm_high,
        }
    }
}

#[derive(Serialize)]
struct ChartResponse {
    hostname: String,
    line: u8,
    granularity: &'static str,
    /// The bucket actually used, as one of [`AGG_PILLS`] — the UI lights up
    /// that pill, which is how a downgraded request stays honest on screen.
    agg_effective: &'static str,
    points: Vec<ChartPoint>,
    thresholds: Option<ThresholdsJson>,
    sensor_name: Option<String>,
    unit: &'static str,
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

async fn chart_json(
    _current_user: CurrentUser,
    State(state): State<AppState>,
    Path(line): Path<u8>,
    Query(q): Query<ChartQuery>,
) -> Response {
    if line > 7 {
        return json_error(StatusCode::NOT_FOUND, "unknown sensor line");
    }

    let (hostname, sensor_name, thresholds) = {
        let portal_state = state.store.snapshot();
        // Hub-scoped and not under `/hubs/{hub}`: `?hub=`, else the only Hub,
        // else nothing to read (devices.hub.md §12 E, `scoped_hub`).
        match portal_state.scoped_hub(q.hub.as_deref()) {
            Some(device) => {
                let sensor = &device.sensors[line as usize];
                (
                    device.hostname.clone(),
                    sensor.name.clone(),
                    sensor.thresholds.as_ref().map(ThresholdsJson::from),
                )
            }
            None => {
                tracing::warn!(
                    hub = q.hub.as_deref(),
                    hubs = portal_state.devices.len(),
                    line,
                    "wired chart could not resolve a Hub; pass ?hub=<hostname>"
                );
                return json_error(StatusCode::NOT_FOUND, "no device");
            }
        }
    };

    let now = now_unix();
    let (from, to) = match resolve_range(&q, now) {
        Ok(range) => range,
        Err(message) => return json_error(StatusCode::BAD_REQUEST, message),
    };

    let agg = Agg::parse(q.agg.as_deref());
    let span = to - from;
    // `auto` keeps the span-driven tier; an explicit bucket only needs raw
    // rows when it is finer than a minute.
    let preferred = match agg {
        Agg::Auto => granularity_for_span(span),
        Agg::Raw | Agg::S30 => Granularity::Raw,
        _ => Granularity::Minute,
    };
    let line_i64 = i64::from(line);

    let read = |g: Granularity| {
        let hostname = hostname.clone();
        let state = state.clone();
        async move {
            match g {
                Granularity::Raw => state
                    .db
                    .run(move |s| s.query_raw(&hostname, line_i64, from, to))
                    .await
                    .map(|rows| rows.iter().map(ChartPoint::from).collect::<Vec<_>>()),
                Granularity::Minute => state
                    .db
                    .run(move |s| s.query_minutes(&hostname, line_i64, from, to))
                    .await
                    .map(|rows| rows.iter().map(ChartPoint::from).collect::<Vec<_>>()),
                Granularity::Hour => state
                    .db
                    .run(move |s| s.query_hourly(&hostname, line_i64, from, to))
                    .await
                    .map(|rows| rows.iter().map(ChartPoint::from).collect::<Vec<_>>()),
            }
        }
    };

    let mut granularity = preferred;
    let mut points = match read(preferred).await {
        Ok(points) => points,
        Err(err) => {
            tracing::error!(%err, hostname, line, "chart query failed");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "query failed");
        }
    };
    // Tier fallback (module header): the minute tier is a separate export
    // stream from the live readings, so "empty" there does not mean "no
    // data" — look in the other tier before answering with nothing.
    if points.is_empty() {
        let fallback = match preferred {
            Granularity::Raw => Granularity::Minute,
            Granularity::Minute | Granularity::Hour => Granularity::Raw,
        };
        if let Ok(rows) = read(fallback).await {
            if !rows.is_empty() {
                granularity = fallback;
                points = rows;
            }
        }
    }

    let (points, agg_effective) = apply_agg(points, agg, granularity.grain_secs(), span);

    Json(ChartResponse {
        hostname,
        line,
        granularity: granularity.as_str(),
        agg_effective,
        points,
        thresholds,
        sensor_name,
        unit: "\u{b0}C",
    })
    .into_response()
}

// ---------------------------------------------------------------------
// GET /export/sensor/{line}.csv
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct ExportQuery {
    from: Option<String>,
    to: Option<String>,
}

/// Parses a `from`/`to` export query param. Accepts full RFC3339
/// (`…Z`/`±HH:MM` offset) as-is. Also accepts a bare `datetime-local` value
/// with no timezone (`YYYY-MM-DDTHH:MM` or `…:SS`) — exactly what
/// `templates/pages/history.html`'s export form's `<input
/// type="datetime-local">` submits (see that template's header comment) —
/// treated as UTC, per this phase's single-Hub, no-per-viewer-timezone
/// stance.
///
/// `pub(crate)`: reused by `routes::node_routes`'s backfill request form,
/// which submits the same `datetime-local` shape (M5-G, "reuse the M3
/// parser" rather than duplicating this).
pub(crate) fn parse_iso8601(s: &str) -> Result<i64, ()> {
    if let Ok(dt) = OffsetDateTime::parse(s, &Rfc3339) {
        return Ok(dt.unix_timestamp());
    }
    let normalized = match s.len() {
        16 => format!("{s}:00Z"), // "YYYY-MM-DDTHH:MM"
        19 => format!("{s}Z"),    // "YYYY-MM-DDTHH:MM:SS"
        _ => return Err(()),
    };
    OffsetDateTime::parse(&normalized, &Rfc3339)
        .map(|dt| dt.unix_timestamp())
        .map_err(|_| ())
}

pub(crate) fn format_rfc3339(unix_secs: i64) -> String {
    OffsetDateTime::from_unix_timestamp(unix_secs)
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .unwrap_or_default()
}

pub(crate) fn today_yyyymmdd() -> String {
    let now = OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    )
}

/// Wraps a field in double quotes (doubling any embedded `"`) when it
/// contains a comma, quote, or newline — otherwise returned verbatim.
/// Matches Python's `csv` module default (`excel`) dialect, which every
/// value here (hostname, ISO timestamp, numbers) is expected to satisfy
/// without quoting in practice.
pub(crate) fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// v1's `csv.writer` (the `excel` dialect) terminates rows with `\r\n`.
pub(crate) const CSV_HEADER: &str =
    "timestamp,device,sensor_line,dev_eui,field,value,temperature,min_celsius,max_celsius\r\n";

/// Builds the exact CSV body v1's `export_sensor` produces for a wired
/// sensor (module header comment has the full semantics/deviation notes).
fn build_export_csv(hostname: &str, line: u8, rows: &[MinuteReading]) -> String {
    let mut out = String::with_capacity(CSV_HEADER.len() + rows.len() * 64);
    out.push_str(CSV_HEADER);
    for row in rows {
        let ts = format_rfc3339(row.minute_ts);
        let avg = row.avg_c.map(|v| v.to_string()).unwrap_or_default();
        let min = row.min_c.map(|v| v.to_string()).unwrap_or_default();
        let max = row.max_c.map(|v| v.to_string()).unwrap_or_default();
        out.push_str(&format!(
            "{},{},{},,temperature,{},{},{},{}\r\n",
            csv_field(&ts),
            csv_field(hostname),
            line,
            csv_field(&avg),
            csv_field(&avg),
            csv_field(&min),
            csv_field(&max),
        ));
    }
    out
}

async fn export_csv(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path(line_csv): Path<String>,
    Query(q): Query<ExportQuery>,
) -> Response {
    // axum/matchit can't mix a literal suffix into a `{param}` segment, so
    // the route captures the whole `{line}.csv` segment and this strips the
    // suffix itself.
    let Some(line_str) = line_csv.strip_suffix(".csv") else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Ok(line) = line_str.parse::<u8>() else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    if line > 7 {
        return (StatusCode::NOT_FOUND, "unknown sensor line").into_response();
    }

    let (Some(from_raw), Some(to_raw)) = (q.from.as_deref(), q.to.as_deref()) else {
        return (StatusCode::BAD_REQUEST, "'from' and 'to' are required").into_response();
    };
    let Some(from_ts) = crate::display_tz::parse_local(from_raw) else {
        return (StatusCode::BAD_REQUEST, "invalid 'from' date").into_response();
    };
    let Some(to_ts) = crate::display_tz::parse_local(to_raw) else {
        return (StatusCode::BAD_REQUEST, "invalid 'to' date").into_response();
    };
    if to_ts <= from_ts {
        return (StatusCode::BAD_REQUEST, "'to' must be after 'from'").into_response();
    }

    let hostname = {
        let portal_state = state.store.snapshot();
        portal_state
            .devices
            .values()
            .next()
            .map(|d| d.hostname.clone())
    };
    let Some(hostname) = hostname else {
        return (StatusCode::NOT_FOUND, "no device").into_response();
    };

    // `query_minutes` is `[from, to)`; v1's SQL bound is inclusive on both
    // ends (`temperature_readings` >= from AND <= to) — `to_ts + 1` restores
    // that inclusive-upper-bound behavior for second-aligned minute slots.
    let rows = match ({
        let a0 = hostname.clone();
        let a1 = i64::from(line);
        let a3 = to_ts + 1;
        state.db.run(move |s| s.query_minutes(&a0, a1, from_ts, a3))
    })
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(%err, hostname, line, "export query failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query failed").into_response();
        }
    };

    let body = build_export_csv(&hostname, line, &rows);
    let filename = format!("sensor_{hostname}_{line}_{}.csv", today_yyyymmdd());

    let details = serde_json::json!({
        "hostname": hostname,
        "from": from_ts,
        "rows": rows.len(),
    })
    .to_string();
    state
        .auth
        .audit(
            "export",
            Some(&current_user),
            None,
            Some(&line.to_string()),
            Some(details),
            true,
        )
        .await;

    let mut response = (StatusCode::OK, body).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/csv"));
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"));
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, disposition);
    response
}

// ---------------------------------------------------------------------
// GET /export/history.csv
// ---------------------------------------------------------------------

/// The History page's download icon: the same columns as
/// `/export/sensor/{line}.csv` (which stays as it was — v1 parity, and the
/// sensor cards still link to it) but for every sensor in `?s=`, wired and
/// Node alike, over the charted window. Node rows put the node key in
/// `dev_eui` and the field name in `field`, exactly as v1's own multi-sensor
/// export does.
#[derive(Deserialize)]
struct HistoryExportQuery {
    s: Option<String>,
    range: Option<String>,
    from: Option<String>,
    to: Option<String>,
}

async fn export_history_csv(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Query(q): Query<HistoryExportQuery>,
) -> Response {
    let first_hub = state
        .store
        .snapshot()
        .devices
        .values()
        .next()
        .map(|d| d.hostname.clone());
    let selection = parse_selection(q.s.as_deref().unwrap_or(""), first_hub.as_deref());
    if selection.is_empty() {
        return (StatusCode::BAD_REQUEST, "no sensors selected").into_response();
    }

    let hostname = {
        let portal_state = state.store.snapshot();
        portal_state
            .devices
            .values()
            .next()
            .map(|d| d.hostname.clone())
    };
    let Some(hostname) = hostname else {
        return (StatusCode::NOT_FOUND, "no device").into_response();
    };

    let now = now_unix();
    let (from_ts, to_ts) = match resolve_range_parts(
        q.range.as_deref(),
        q.from.as_deref().and_then(crate::display_tz::parse_local),
        q.to.as_deref().and_then(crate::display_tz::parse_local),
        now,
    ) {
        Ok(bounds) => bounds,
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    };

    let mut body = String::from(CSV_HEADER);
    for sel in &selection {
        match sel {
            Selection::Line { hub, line } => {
                // The probe's own Hub, as the token names it.
                let hostname = state
                    .store
                    .snapshot()
                    .devices
                    .values()
                    .find(|d| d.hostname.eq_ignore_ascii_case(hub))
                    .map(|d| d.hostname.clone())
                    .unwrap_or_else(|| hostname.clone());
                let Ok(rows) = ({
                    let a0 = hostname.clone();
                    let a1 = i64::from(*line);
                    let a3 = to_ts + 1;
                    state.db.run(move |s| s.query_minutes(&a0, a1, from_ts, a3))
                })
                .await
                else {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "query failed").into_response();
                };
                body.push_str(
                    build_export_csv(&hostname, *line, &rows).trim_start_matches(CSV_HEADER),
                );
            }
            Selection::NodeField { node_key, field } => {
                // A count is stored with the counters (GitLab #134).
                let is_count = fiber_protocol::lorawan_field(field)
                    .is_some_and(|f| f.kind == fiber_protocol::LorawanFieldKind::Counter);
                let Ok(rows) = ({
                    let a0 = hostname.clone();
                    let a1 = node_key.clone();
                    let a2 = field.clone();
                    let a4 = to_ts + 1;
                    state.db.run(move |s| {
                        super::node_routes::node_series(s, &a0, &a1, &a2, is_count, from_ts, a4)
                    })
                })
                .await
                else {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "query failed").into_response();
                };
                // The stored wire value, not the displayed one: pressure stays
                // in Pa here, because a CSV is machine data (UX-038).
                for &(ts, value) in &rows {
                    let value = value.map(|v| v.to_string()).unwrap_or_default();
                    body.push_str(&format!(
                        "{},{},,{},{},{},,,\r\n",
                        csv_field(&format_rfc3339(ts)),
                        csv_field(&hostname),
                        csv_field(node_key),
                        csv_field(field),
                        csv_field(&value),
                    ));
                }
            }
            Selection::EyeField { mac, field } => {
                // The Beacon's own Hub; the MAC goes in the `dev_eui` column.
                let host = state
                    .store
                    .snapshot()
                    .hub_of_beacon(mac)
                    .map(|(d, _)| d.hostname.clone())
                    .unwrap_or_else(|| hostname.clone());
                let Ok(rows) = ({
                    let (a0, a1) = (host.clone(), mac.clone());
                    let a4 = to_ts + 1;
                    state
                        .db
                        .run(move |s| s.query_eye_readings(&a0, &a1, from_ts, a4, None))
                })
                .await
                else {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "query failed").into_response();
                };
                for r in &rows {
                    let value = match field.as_str() {
                        "temperature" => r.temperature_c,
                        _ => r.humidity_pct.map(|h| h as f64),
                    };
                    let Some(value) = value else { continue };
                    body.push_str(&format!(
                        "{},{},,{},{},{},,,\r\n",
                        csv_field(&format_rfc3339(r.ts)),
                        csv_field(&host),
                        csv_field(mac),
                        csv_field(field),
                        csv_field(&value.to_string()),
                    ));
                }
            }
        }
    }

    let filename = format!("history_{hostname}_{}.csv", today_yyyymmdd());
    // HS-11 (Hub sweep 2026-09-26): the row said "export" and nothing else. It names the Hub,
    // the window, what was selected and how many rows went out, as the group and probe
    // exports do.
    let details = serde_json::json!({
        "from": from_ts,
        "to": to_ts,
        "selections": selection.len(),
        "rows": body.lines().count().saturating_sub(1),
    })
    .to_string();
    state
        .auth
        .audit(
            "export",
            Some(&current_user),
            None,
            Some(&hostname),
            Some(details),
            true,
        )
        .await;

    let mut response = (StatusCode::OK, body).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/csv"));
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"));
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, disposition);
    response
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trend_series(hostname: &str, prefix: &str, field: Option<&str>) -> SelectedCtx {
        SelectedCtx {
            name: "Fridge (Temperature)".into(),
            hostname: hostname.into(),
            color: SERIES_COLORS[0],
            value: None,
            unit: String::new(),
            decimals: 1,
            state: "Normal".into(),
            thresholds: None,
            last_seen: None,
            chart_url: String::new(),
            alarm_prefix: prefix.into(),
            alarm_field: field.map(str::to_string),
            node_key: field.map(|_| "aabb".to_string()),
        }
    }

    /// `ev` / `evt`: off by default, all three kinds unless narrowed, and
    /// the query suffix round-trips.
    #[test]
    fn trend_event_options_parse_and_round_trip() {
        let off = EventOpts::parse(None, None);
        assert!(!off.on);
        assert_eq!(off.qs(), "");
        let all = EventOpts::parse(Some("1"), None);
        assert_eq!(all.kinds, [true; 3]);
        assert_eq!(all.qs(), "&ev=1");
        let some = EventOpts::parse(Some("1"), Some("log,alarm,bogus"));
        assert_eq!(some.kinds, [true, false, true]);
        assert_eq!(some.qs(), "&ev=1&evt=alarm,log");
        assert_eq!(EventOpts::parse(Some("1"), Some("bogus")).kinds, [true; 3]);
        // A viewer never reads the log (decision #18).
        assert_eq!(all.effective(false), [true, true, false]);
        assert_eq!(all.effective(true), [true; 3]);
        // The dropdown: shown kinds, and none shown = off.
        assert_eq!(off.shown(true), [false; 3]);
        assert_eq!(all.shown(false), [true, true, false]);
        assert_eq!(EventOpts::from_shown([false; 3]).qs(), "");
        assert_eq!(
            EventOpts::from_shown([false, true, false]).qs(),
            "&ev=1&evt=warning"
        );
        assert_eq!(EventOpts::from_shown([true; 3]).qs(), "&ev=1");
    }

    /// An alarm belongs to a channel, not just to its probe: a wired line
    /// on its own Hub, a Node field (or the Node's liveness) only.
    #[test]
    fn trend_events_match_the_selected_channel_only() {
        let wired = trend_series("h1", "line-0", None);
        assert!(alarm_on_channel(&wired, "h1", "line-0"));
        assert!(!alarm_on_channel(&wired, "h2", "line-0"), "another Hub");
        assert!(!alarm_on_channel(&wired, "h1", "line-1"));
        let node = trend_series("h1", "lrw:aabb:", Some("temperature"));
        assert!(alarm_on_channel(&node, "h1", "lrw:aabb:temperature"));
        assert!(alarm_on_channel(&node, "h1", "lrw:aabb:liveness"));
        assert!(
            alarm_on_channel(&node, "h1", "lrw:aabb:Temperature"),
            "the stored id carries the field's label"
        );
        assert!(!alarm_on_channel(&node, "h1", "lrw:aabb:humidity"));
        assert!(!alarm_on_channel(&node, "h1", "lrw:aabbcc:temperature"));
    }

    /// Log entries count when their target is the Node, not when the key
    /// merely appears in another column.
    #[test]
    fn trend_log_events_take_the_node_target() {
        let entry = |target: &str| portal_storage::AuditEntry {
            id: 1,
            ts: "2026-10-15 08:00:00".into(),
            event_type: "node_rename".into(),
            username: Some("admin".into()),
            user_id: Some(1),
            ip: None,
            target: Some(target.into()),
            details: None,
            success: true,
        };
        let owner = trend_series("h1", "lrw:aabb:", Some("temperature"));
        let evs = log_events(&[entry("AABB"), entry("aabbcc")], "aabb", &owner);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].kind, "log");
        assert_eq!(evs[0].item, "Fridge");
        assert_eq!(evs[0].message, "node_rename (admin)");
        assert_eq!(evs[0].ts, parse_iso8601("2026-10-15T08:00:00Z").unwrap());
        assert!(evs[0].href.starts_with("/audit?type=log&q=aabb"));
        assert_eq!(audit_ts(evs[0].ts), "2026-10-15 08:00:00");
    }

    /// The query end to end: the window and the severity filters apply, and
    /// an unselected probe's alarm stays out.
    #[tokio::test]
    async fn trend_events_read_the_window_and_the_kinds() {
        let (state, dir) = crate::routes::test_support::test_state().await;
        let storage =
            portal_storage::Storage::open(&dir.path().join("portal.db"), None).expect("storage");
        for (sensor, sev, at) in [
            ("line-0", "critical", "2026-10-15T08:00:00Z"),
            ("line-1", "warning", "2026-10-15T08:30:00Z"),
            ("line-2", "critical", "2026-10-15T09:00:00Z"),
            ("line-3", "critical", "2026-10-10T09:00:00Z"),
        ] {
            storage
                .insert_alarm_event(&portal_storage::NewAlarmEvent {
                    hostname: "h1".into(),
                    sensor_id: sensor.into(),
                    sensor_name: None,
                    sensor_location: None,
                    alarm_type: "HIGH".into(),
                    severity: sev.into(),
                    from_state: Some("NORMAL".into()),
                    to_state: Some(sev.to_uppercase()),
                    message: Some(format!("{sensor} high")),
                    value: Some(9.0),
                    threshold: Some(8.0),
                    triggered_at: at.into(),
                    cleared_at: None,
                    acknowledged_at: None,
                    acknowledged_by_user_id: None,
                    acknowledged_by_username: None,
                    native_key: None,
                })
                .expect("seed");
        }
        let sel = [
            trend_series("h1", "line-0", None),
            trend_series("h1", "line-1", None),
            trend_series("h1", "line-3", None),
        ];
        let from = parse_iso8601("2026-10-15T00:00:00Z").unwrap();
        let to = parse_iso8601("2026-10-15T23:59:59Z").unwrap();
        let evs = trend_events(&state, &sel, from, to, [true, true, false]).await;
        let msgs: Vec<&str> = evs.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(
            msgs,
            ["line-1 high", "line-0 high"],
            "newest first, in window"
        );
        assert_eq!(evs[0].kind, "warning");
        assert_eq!(evs[1].kind, "alarm");
        assert!(evs[1].href.starts_with("/alarms/"));
        let only_alarms = trend_events(&state, &sel, from, to, [true, false, false]).await;
        assert_eq!(only_alarms.len(), 1);
        assert!(trend_events(&state, &[], from, to, [true; 3])
            .await
            .is_empty());
    }

    #[test]
    fn parse_iso8601_accepts_datetime_local_and_rfc3339() {
        // `<input type="datetime-local">`'s no-seconds shape.
        assert_eq!(
            parse_iso8601("2026-08-18T14:30"),
            Ok(OffsetDateTime::parse("2026-08-18T14:30:00Z", &Rfc3339)
                .unwrap()
                .unix_timestamp())
        );
        // With seconds, still no offset.
        assert_eq!(
            parse_iso8601("2026-08-18T14:30:05"),
            Ok(OffsetDateTime::parse("2026-08-18T14:30:05Z", &Rfc3339)
                .unwrap()
                .unix_timestamp())
        );
        // Full RFC3339 passes straight through.
        assert_eq!(
            parse_iso8601("2026-08-18T14:30:05Z"),
            Ok(OffsetDateTime::parse("2026-08-18T14:30:05Z", &Rfc3339)
                .unwrap()
                .unix_timestamp())
        );
        assert!(parse_iso8601("not-a-date").is_err());
    }

    #[test]
    fn granularity_policy_thresholds() {
        assert_eq!(granularity_for_span(0), Granularity::Raw);
        assert_eq!(granularity_for_span(TWO_HOURS_SECS), Granularity::Raw);
        assert_eq!(
            granularity_for_span(TWO_HOURS_SECS + 1),
            Granularity::Minute
        );
        assert_eq!(
            granularity_for_span(FORTY_EIGHT_HOURS_SECS),
            Granularity::Minute
        );
        assert_eq!(
            granularity_for_span(FORTY_EIGHT_HOURS_SECS + 1),
            Granularity::Hour
        );
    }

    #[test]
    fn resolve_range_prefers_explicit_bounds() {
        let q = ChartQuery {
            range: Some("7d".to_string()),
            from: Some(100),
            to: Some(200),
            agg: None,
            hub: None,
        };
        assert_eq!(resolve_range(&q, 1_000).unwrap(), (100, 200));
    }

    #[test]
    fn resolve_range_rejects_non_increasing_bounds() {
        let q = ChartQuery {
            range: None,
            from: Some(200),
            to: Some(200),
            agg: None,
            hub: None,
        };
        assert!(resolve_range(&q, 1_000).is_err());
    }

    #[test]
    fn resolve_range_named_ranges() {
        let now = 1_000_000;
        for (name, secs) in [
            ("1h", 3600),
            ("6h", 6 * 3600),
            ("24h", 24 * 3600),
            ("7d", 7 * 24 * 3600),
            ("30d", 30 * 24 * 3600),
            ("1y", 365 * 24 * 3600),
        ] {
            let q = ChartQuery {
                range: Some(name.to_string()),
                from: None,
                to: None,
                agg: None,
                hub: None,
            };
            assert_eq!(resolve_range(&q, now).unwrap(), (now - secs, now), "{name}");
        }
    }

    #[test]
    fn resolve_range_defaults_to_24h() {
        let now = 1_000_000;
        let q = ChartQuery {
            range: None,
            from: None,
            to: None,
            agg: None,
            hub: None,
        };
        assert_eq!(resolve_range(&q, now).unwrap(), (now - 24 * 3600, now));
        let q_unknown = ChartQuery {
            range: Some("bogus".to_string()),
            from: None,
            to: None,
            agg: None,
            hub: None,
        };
        assert_eq!(
            resolve_range(&q_unknown, now).unwrap(),
            (now - 24 * 3600, now)
        );
    }

    #[test]
    fn agg_parses_every_pill_and_defaults_to_auto() {
        for name in AGG_PILLS {
            assert_eq!(Agg::parse(Some(name)).as_str(), name, "pill {name}");
        }
        assert_eq!(Agg::parse(None), Agg::Auto);
        assert_eq!(Agg::parse(Some("auto")), Agg::Auto);
        // Anything unrecognized is `auto`, not an error: this comes off a
        // query string an operator can edit.
        assert_eq!(Agg::parse(Some("5s")), Agg::Auto);
    }

    fn point(ts: i64, avg: f64, min: f64, max: f64) -> ChartPoint {
        ChartPoint {
            ts,
            avg: Some(avg),
            min: Some(min),
            max: Some(max),
        }
    }

    #[test]
    fn bucket_points_averages_avg_and_spans_min_max() {
        // Three points inside the 0..60 slot, two inside 60..120.
        let points = vec![
            point(0, 10.0, 9.0, 11.0),
            point(20, 20.0, 15.0, 25.0),
            point(59, 30.0, 29.0, 41.0),
            point(60, 4.0, 3.0, 5.0),
            point(119, 6.0, 1.0, 7.0),
        ];
        let out = bucket_points(points, 60);
        assert_eq!(out.len(), 2);

        assert_eq!(out[0].ts, 0);
        assert_eq!(out[0].avg, Some(20.0)); // (10 + 20 + 30) / 3
        assert_eq!(out[0].min, Some(9.0));
        assert_eq!(out[0].max, Some(41.0));

        assert_eq!(out[1].ts, 60);
        assert_eq!(out[1].avg, Some(5.0)); // (4 + 6) / 2
        assert_eq!(out[1].min, Some(1.0));
        assert_eq!(out[1].max, Some(7.0));
    }

    #[test]
    fn bucket_points_skips_empty_slots_and_is_identity_at_zero() {
        // A 10-minute hole between the two points must stay a hole, not
        // become a run of zero-valued buckets.
        let points = vec![point(0, 1.0, 1.0, 1.0), point(600, 2.0, 2.0, 2.0)];
        let out = bucket_points(points, 60);
        assert_eq!(out.iter().map(|p| p.ts).collect::<Vec<_>>(), vec![0, 600]);

        let points = vec![point(7, 1.0, 1.0, 1.0)];
        assert_eq!(bucket_points(points, 0)[0].ts, 7);
    }

    #[test]
    fn bucket_points_keeps_min_max_when_avg_is_missing() {
        let points = vec![
            ChartPoint {
                ts: 0,
                avg: None,
                min: Some(2.0),
                max: Some(3.0),
            },
            point(30, 8.0, 1.0, 9.0),
        ];
        let out = bucket_points(points, 60);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].avg, Some(8.0)); // only the point that had one
        assert_eq!(out[0].min, Some(1.0));
        assert_eq!(out[0].max, Some(9.0));
    }

    #[test]
    fn apply_agg_cannot_go_finer_than_the_stored_series() {
        let points = vec![point(0, 1.0, 1.0, 1.0), point(60, 2.0, 2.0, 2.0)];
        // 30s buckets asked for, but the rows are minute rollups.
        let (out, effective) = apply_agg(points.clone(), Agg::S30, 60, 3600);
        assert_eq!(effective, "1m");
        assert_eq!(out.len(), 2);

        // The same request over raw rows keeps the 30s bucket.
        let (_, effective) = apply_agg(points, Agg::S30, 1, 3600);
        assert_eq!(effective, "30s");
    }

    #[test]
    fn apply_agg_escalates_the_bucket_past_the_point_cap() {
        let year = 365 * 24 * 3600;
        let (_, effective) = apply_agg(Vec::new(), Agg::M1, 1, year);
        assert_eq!(effective, "1d", "a year of minute buckets is 525k points");

        // Auto is left alone as long as the tier's own density fits.
        let points = vec![point(0, 1.0, 1.0, 1.0)];
        let (out, effective) = apply_agg(points, Agg::Auto, 60, 24 * 3600);
        assert_eq!((out.len(), effective), (1, "1m"));
    }

    #[test]
    fn selection_round_trips_through_its_token() {
        let raw = "line:0,node:00AA:temperature,nonsense,line:0,line:9";
        let parsed = parse_selection(raw, Some("Hub-A"));
        assert_eq!(
            parsed.iter().map(Selection::token).collect::<Vec<_>>(),
            vec!["line:hub-a:0", "node:00aa:temperature"],
            "duplicates, junk and out-of-range lines are dropped"
        );
        assert_eq!(parsed[0].chart_url(), "/api/v1/chart/0");
        assert_eq!(parsed[1].chart_url(), "/api/v1/chart/node/00aa/temperature");
    }

    /// ui_components.md §2: the Channels card links Beacon channels as
    /// `eye:{MAC}:{field}`, the MAC keeping its colons.
    #[test]
    fn beacon_tokens_parse_with_their_colons() {
        let parsed = parse_selection(
            "eye:7c:d9:f4:00:00:01:humidity,eye:7C:D9:F4:00:00:01:battery,eye:bad:temperature",
            None,
        );
        assert_eq!(
            parsed,
            vec![Selection::EyeField {
                mac: "7C:D9:F4:00:00:01".into(),
                field: "humidity".into()
            }],
            "an unstored field and a malformed MAC are dropped"
        );
        assert_eq!(parsed[0].token(), "eye:7C:D9:F4:00:00:01:humidity");
        assert_eq!(
            parsed[0].chart_url(),
            "/api/v1/chart/eye/7C:D9:F4:00:00:01/humidity"
        );
    }

    /// devices.hub.md amendment 2026-10-15: a wired token names its Hub, and
    /// the old `line:{n}` (+ `?hub=`) still resolves to the same probe.
    #[test]
    fn wired_tokens_carry_the_hub_and_old_ones_still_parse() {
        let line = |hub: &str, line| Selection::Line {
            hub: hub.into(),
            line,
        };
        // New shape, independent of any default Hub.
        assert_eq!(
            parse_selection("line:hub-b:3,line:HUB-A:3", None),
            vec![line("hub-b", 3), line("hub-a", 3)],
            "the same line on two Hubs is two probes"
        );
        // Legacy shape: the default Hub (`?hub=` or the only Hub) fills in.
        assert_eq!(
            parse_selection("line:3", Some("hub-a")),
            vec![line("hub-a", 3)]
        );
        // Legacy shape, several Hubs and no `?hub=`: nothing to resolve to.
        assert!(parse_selection("line:3", None).is_empty());
        // Legacy and new naming the same probe collapse into one.
        assert_eq!(
            parse_selection("line:3,line:hub-a:3", Some("hub-a")).len(),
            1
        );
        assert!(parse_selection("line:hub-a:8,line::1,line:a:b:1", None).is_empty());
        assert_eq!(line("hub-a", 3).token(), "line:hub-a:3");
    }

    #[test]
    fn toggling_adds_at_the_end_and_removes_in_place() {
        let mk = |n: u8| Selection::Line {
            hub: "h".into(),
            line: n,
        };
        let a = mk(0);
        let b = mk(1);
        let selection = vec![a.clone(), b.clone()];
        // Removing keeps the survivor's index, so its colour does not move.
        assert_eq!(toggled(&selection, &a), vec![b.clone()]);
        // Adding appends, taking the next free colour.
        assert_eq!(
            toggled(std::slice::from_ref(&a), &b),
            vec![a.clone(), b.clone()]
        );
        // A full selection refuses more rather than silently dropping one.
        let full: Vec<Selection> = (0..MAX_SENSORS as u8).map(mk).collect();
        assert_eq!(
            toggled(
                &full,
                &Selection::NodeField {
                    node_key: "aa".into(),
                    field: "temperature".into(),
                }
            ),
            full
        );
    }

    #[test]
    fn export_csv_golden_header_and_row() {
        let rows = vec![MinuteReading {
            hostname: "hub1".to_string(),
            sensor_line: 3,
            minute_ts: 1_700_000_000, // 2023-11-14T22:13:20Z
            min_c: Some(-18.7),
            avg_c: Some(-18.2),
            max_c: Some(-17.9),
            sample_count: Some(60),
            disconnect_count: Some(0),
            worst_alarm: None,
            source: portal_storage::SOURCE_LIVE.to_string(),
            data_hmac: None,
        }];

        let csv = build_export_csv("hub1", 3, &rows);
        let mut lines = csv.split("\r\n");
        assert_eq!(
            lines.next().unwrap(),
            "timestamp,device,sensor_line,dev_eui,field,value,temperature,min_celsius,max_celsius"
        );
        assert_eq!(
            lines.next().unwrap(),
            "2023-11-14T22:13:20Z,hub1,3,,temperature,-18.2,-18.2,-18.7,-17.9"
        );
        assert_eq!(lines.next().unwrap(), "");
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn export_csv_blank_cells_for_missing_values() {
        let rows = vec![MinuteReading {
            hostname: "hub1".to_string(),
            sensor_line: 0,
            minute_ts: 0,
            min_c: None,
            avg_c: None,
            max_c: None,
            sample_count: None,
            disconnect_count: None,
            worst_alarm: None,
            source: portal_storage::SOURCE_LIVE.to_string(),
            data_hmac: None,
        }];

        let csv = build_export_csv("hub1", 0, &rows);
        let row = csv.lines().nth(1).unwrap();
        assert_eq!(row, "1970-01-01T00:00:00Z,hub1,0,,temperature,,,,");
    }
}

/// End-to-end tests through the real router (auth gate, JSON/CSV wiring) —
/// separate module from the pure-function tests above since these need a
/// tempdir-backed `AppState`, mirroring `http::tests::test_app`.
#[cfg(test)]
mod route_tests {
    use std::path::Path;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use fiber_protocol::payloads::status::DeviceStatus;
    use portal_storage::{MinuteReading, Storage, SOURCE_LIVE};
    use tower::ServiceExt;

    use crate::auth::{AuthService, SESSION_COOKIE};
    use crate::config::PortalConfig;
    use crate::db::Db;
    use crate::http::app;
    use crate::relay::CommandRelay;
    use crate::state::{IngestEvent, StateStore};

    use super::now_unix;

    /// Returns `(router, tempdir guard, session token, data_dir)` — the
    /// store already has one online device ("hub1", all 8 lines
    /// pre-created); `data_dir` lets a test open its own `Storage` on the
    /// same `portal.db` to seed rows the request-path `Db` can then see
    /// (WAL allows the extra writer/reader connection).
    async fn test_app() -> (axum::Router, tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8 path").to_string();

        let auth = AuthService::open(&data_dir, None, crate::durable::test_gate(&data_dir))
            .expect("open auth service");
        auth.create_superuser(
            "admin".to_string(),
            "correct-horse-battery".to_string(),
            None,
        )
        .await
        .expect("create superuser");
        let (token, _) = auth
            .login(
                "admin".to_string(),
                "correct-horse-battery".to_string(),
                None,
            )
            .await
            .expect("login");

        let db = Db::open(&data_dir, None, "test").expect("open db");
        let store = Arc::new(StateStore::new());
        let calibration = crate::calibration::CalibrationService::test_instance(
            db.clone(),
            &data_dir,
            Arc::clone(&store),
        );
        store.apply(IngestEvent::Status {
            hostname: "hub1".to_string(),
            status: DeviceStatus::Online,
        });

        let data_dir_for_seeding = data_dir.clone();
        let config = PortalConfig {
            bind_addr: "127.0.0.1:0".to_string(),
            mqtt_host: "localhost".to_string(),
            mqtt_port: 1883,
            mqtt_username: None,
            mqtt_password: None,
            mqtt_client_id: "proximos-portal".into(),
            topic_prefix: "proximos".to_string(),
            data_dir,
            db_key: None,
            templates_dir: "templates".to_string(),
            static_dir: "static".to_string(),
            cookie_secure: false,
            local_hostname: None,
            trusted_proxies: Vec::new(),
            experimental_features_enabled: false,
            nodes: Default::default(),
            beacons: Default::default(),
            signal_quality: Default::default(),
            thresholds: Default::default(),
            calibration: Default::default(),
            maintenance_window: None,
        };

        let keys =
            crate::keys::KeyService::open(&data_dir_for_seeding, None).expect("open key service");
        let backfill = crate::backfill::BackfillService::test_instance(&data_dir_for_seeding);
        let alarms = crate::alarms::AlarmService::test_instance(&data_dir_for_seeding);

        (
            app(
                &config,
                store,
                auth,
                db.clone(),
                crate::durable::test_gate(&data_dir_for_seeding),
                CommandRelay::test_instance(),
                crate::radio::RadioControl::from_env(),
                keys,
                backfill,
                crate::beacon_history::BeaconHistoryService::test_instance(&config.data_dir),
                alarms,
                crate::pending::PendingService::open(&data_dir_for_seeding, None)
                    .expect("open pending service"),
                crate::offline::OfflineService::test_instance(db),
                calibration,
                Some(crate::retention::Maintenance::test_instance(
                    &data_dir_for_seeding,
                )),
                std::sync::Arc::new(crate::sandbox::SandboxStatus {
                    state: crate::sandbox::SandboxState::Unsupported,
                    detail: "test harness".into(),
                }),
                crate::routes::test_support::idle_shutdown(),
                // No ingest loop behind a test router, so no invites to serve.
                None,
            ),
            dir,
            token,
            data_dir_for_seeding,
        )
    }

    fn auth_cookie(token: &str) -> String {
        format!("{SESSION_COOKIE}={token}")
    }

    #[tokio::test]
    async fn anonymous_requests_redirect_to_login() {
        let (app, _dir, _token, _data_dir) = test_app().await;
        for path in ["/trends", "/api/v1/chart/0", "/export/sensor/0.csv"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::SEE_OTHER, "path {path}");
            // The GET keeps its target for after the login (UX-027).
            assert_eq!(
                response.headers().get(header::LOCATION).expect("location"),
                format!("/login?next={}", crate::routes::percent_encode_query(path)).as_str(),
                "path {path}"
            );
        }
    }

    #[tokio::test]
    async fn chart_json_reflects_seeded_minute_data() {
        let (app, _dir, token, data_dir) = test_app().await;

        let db_path = Path::new(&data_dir).join("portal.db");
        let seed = Storage::open(&db_path, None).expect("open seed storage");
        let now = now_unix();
        let minute_ts = (now / 60) * 60 - 600;
        seed.upsert_minute(&MinuteReading {
            hostname: "hub1".to_string(),
            sensor_line: 3,
            minute_ts,
            min_c: Some(-18.7),
            avg_c: Some(-18.2),
            max_c: Some(-17.9),
            sample_count: Some(60),
            disconnect_count: Some(0),
            worst_alarm: None,
            source: SOURCE_LIVE.to_string(),
            data_hmac: None,
        })
        .expect("seed minute row");
        drop(seed);

        // 6h span: > 2h (raw cutoff), <= 48h (minute cutoff) -> minute tier.
        let uri = format!("/api/v1/chart/3?from={}&to={now}", now - 6 * 3600);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::COOKIE, auth_cookie(&token))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");

        assert_eq!(json["hostname"], "hub1");
        assert_eq!(json["line"], 3);
        assert_eq!(json["granularity"], "minute");
        assert_eq!(json["unit"], "\u{b0}C");
        assert!(json["thresholds"].is_null());
        assert!(json["sensor_name"].is_null());
        let points = json["points"].as_array().expect("points array");
        assert_eq!(points.len(), 1);
        assert_eq!(points[0]["avg"], -18.2);
        assert_eq!(points[0]["min"], -18.7);
        assert_eq!(points[0]["max"], -17.9);
        assert_eq!(points[0]["ts"], minute_ts);
    }

    #[tokio::test]
    async fn chart_json_404_for_line_out_of_range() {
        let (app, _dir, token, _data_dir) = test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/chart/9?range=1h")
                    .header(header::COOKIE, auth_cookie(&token))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn export_csv_streams_seeded_rows_with_headers() {
        let (app, _dir, token, data_dir) = test_app().await;

        let db_path = Path::new(&data_dir).join("portal.db");
        let seed = Storage::open(&db_path, None).expect("open seed storage");
        let now = now_unix();
        let minute_ts = (now / 60) * 60 - 600;
        seed.upsert_minute(&MinuteReading {
            hostname: "hub1".to_string(),
            sensor_line: 2,
            minute_ts,
            min_c: Some(1.0),
            avg_c: Some(2.5),
            max_c: Some(4.0),
            sample_count: Some(60),
            disconnect_count: Some(0),
            worst_alarm: None,
            source: SOURCE_LIVE.to_string(),
            data_hmac: None,
        })
        .expect("seed minute row");
        drop(seed);

        let from = super::format_rfc3339(now - 3600);
        let to = super::format_rfc3339(now);
        let uri = format!("/export/sensor/2.csv?from={from}&to={to}");
        let response = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::COOKIE, auth_cookie(&token))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .expect("content-type"),
            "text/csv"
        );
        let disposition = response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .expect("content-disposition")
            .to_str()
            .expect("ascii header");
        assert!(disposition.starts_with("attachment; filename=\"sensor_hub1_2_"));

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8 csv");
        let mut lines = text.split("\r\n");
        assert_eq!(
            lines.next().unwrap(),
            "timestamp,device,sensor_line,dev_eui,field,value,temperature,min_celsius,max_celsius"
        );
        assert_eq!(
            lines.next().unwrap(),
            format!(
                "{},hub1,2,,temperature,2.5,2.5,1,4",
                super::format_rfc3339(minute_ts)
            )
        );
    }

    #[tokio::test]
    async fn export_csv_bad_dates_are_400() {
        let (app, _dir, token, _data_dir) = test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/export/sensor/0.csv?from=not-a-date&to=2024-01-01T00:00:00Z")
                    .header(header::COOKIE, auth_cookie(&token))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// `templates/pages/history.html`'s export form submits `datetime-local`
    /// values (`YYYY-MM-DDTHH:MM`, no seconds/timezone) — a real request
    /// from that page must not 400.
    #[tokio::test]
    async fn export_csv_accepts_datetime_local_query_params() {
        let (app, _dir, token, _data_dir) = test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/export/sensor/0.csv?from=2026-08-18T00:00&to=2026-08-18T14:30")
                    .header(header::COOKIE, auth_cookie(&token))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn history_page_renders_for_authenticated_user() {
        let (app, _dir, token, _data_dir) = test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/trends")
                    .header(header::COOKIE, auth_cookie(&token))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let html = String::from_utf8(body.to_vec()).expect("utf8 html");
        assert!(html.contains("Trends"));
        // The picker renders even with nothing selected; the chart island
        // does not (there is nothing to plot yet).
        assert!(html.contains("Select channels"));
        assert!(html.contains("line:hub1:0"));
        assert!(!html.contains("history-chart-config"));
    }

    /// The old address 308s to /trends and keeps the selection.
    #[tokio::test]
    async fn history_redirects_to_trends_with_query() {
        let (app, _dir, token, _data_dir) = test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/history?s=line:0&range=6h")
                    .header(header::COOKIE, auth_cookie(&token))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            response.headers()[header::LOCATION],
            "/trends?s=line:0&range=6h"
        );
    }

    /// UX-034: a Monitoring group is a one-click selection of its members.
    #[tokio::test]
    async fn history_picker_lists_groups_as_selections() {
        let (app, _dir, token, data_dir) = test_app().await;
        let seed = Storage::open(&Path::new(&data_dir).join("portal.db"), None).expect("open");
        let line = |n| portal_storage::NewSensorGroupMember {
            hostname: "hub1".into(),
            sensor_line: Some(n),
            node_key: None,
            mac: None,
            fields: None,
        };
        seed.create_sensor_group("Freezers", None, &[line(0), line(3)])
            .expect("group");
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/trends")
                    .header(header::COOKIE, auth_cookie(&token))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let html = String::from_utf8(body.to_vec()).expect("utf8 html");
        assert!(html.contains("Freezers"), "{html}");
        assert!(html.contains("value=\"line:hub1:0,line:hub1:3\""), "{html}");
        assert!(html.contains("data-group-select"), "{html}");
        assert!(!html.contains("No groups created yet."), "{html}");
    }

    /// No `?agg=` means 10-minute buckets, and "Auto" is still offered.
    #[tokio::test]
    async fn trends_defaults_to_ten_minute_buckets() {
        let (app, _dir, token, _data_dir) = test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/trends?s=line:0&range=24h")
                    .header(header::COOKIE, auth_cookie(&token))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let html = String::from_utf8(body.to_vec()).expect("utf8 html");
        assert!(html.contains(r#"<option value="10m" selected>"#), "{html}");
        assert!(html.contains(r#"<option value="auto">"#), "{html}");
        assert!(
            html.contains(r#"onchange="this.form.requestSubmit()""#),
            "{html}"
        );
        assert!(html.contains("agg=10m"), "{html}");
        assert!(!html.contains("data-agg-explicit"), "{html}");
        assert!(html.contains(r#"popovertarget="history-events""#), "{html}");
    }

    /// `?s=` drives the whole page: the selection reaches both cards, and
    /// the chart island's config names the endpoints the JS will fetch.
    #[tokio::test]
    async fn history_page_renders_the_selected_sensors() {
        let (app, _dir, token, _data_dir) = test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/trends?s=line:0,line:3&range=6h&agg=1m")
                    .header(header::COOKIE, auth_cookie(&token))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let html = String::from_utf8(body.to_vec()).expect("utf8 html");
        assert!(html.contains("history-chart-config"));
        assert!(html.contains("/api/v1/chart/0"));
        assert!(html.contains("/api/v1/chart/3"));
        assert!(html.contains("2/8"), "the n/N counter");
    }

    /// The Aggregation pills are query params, so the JSON endpoint has to
    /// honour them and say which bucket it actually served.
    #[tokio::test]
    async fn chart_json_reports_the_effective_aggregation() {
        let (app, _dir, token, data_dir) = test_app().await;

        let db_path = Path::new(&data_dir).join("portal.db");
        let seed = Storage::open(&db_path, None).expect("open seed storage");
        let now = now_unix();
        let base = (now / 60) * 60 - 3600;
        for i in 0..10 {
            seed.upsert_minute(&MinuteReading {
                hostname: "hub1".to_string(),
                sensor_line: 1,
                minute_ts: base + i * 60,
                min_c: Some(1.0),
                avg_c: Some(2.0),
                max_c: Some(3.0),
                sample_count: Some(60),
                disconnect_count: Some(0),
                worst_alarm: None,
                source: SOURCE_LIVE.to_string(),
                data_hmac: None,
            })
            .expect("seed minute row");
        }
        drop(seed);

        // 30s buckets over minute rows: the answer must own up to `1m`.
        let uri = format!("/api/v1/chart/1?from={}&to={now}&agg=30s", now - 6 * 3600);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::COOKIE, auth_cookie(&token))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(json["agg_effective"], "1m");
        assert_eq!(json["granularity"], "minute");
        assert_eq!(json["points"].as_array().expect("points").len(), 10);
    }
}

/// The `?hub=` rule for the routes that are Hub-scoped without a `{hub}` in
/// their path (devices.hub.md §12 E, `PortalState::scoped_hub`).
#[cfg(test)]
mod multi_hub_tests {
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use fiber_protocol::payloads::status::DeviceStatus;
    use tower::ServiceExt;

    use crate::auth::SESSION_COOKIE;
    use crate::routes::test_support::{login, test_state};
    use crate::state::IngestEvent;

    async fn chart(router: &axum::Router, uri: &str, token: &str) -> StatusCode {
        router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response")
            .status()
    }

    #[tokio::test]
    async fn the_wired_chart_needs_a_hub_once_there_are_two() {
        let (state, _dir) = test_state().await;
        let token = login(&state, "admin", "correct-horse-battery").await;
        state.store.apply(IngestEvent::Status {
            hostname: "h1".into(),
            status: DeviceStatus::Online,
        });
        let router = super::router().with_state(state.clone());

        // One Hub: no parameter needed, exactly as before.
        assert_eq!(
            chart(&router, "/api/v1/chart/0", &token).await,
            StatusCode::OK
        );

        state.store.apply(IngestEvent::Status {
            hostname: "h2".into(),
            status: DeviceStatus::Online,
        });

        // Two Hubs and no `?hub=`: there is no honest answer, so 404.
        assert_eq!(
            chart(&router, "/api/v1/chart/0", &token).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            chart(&router, "/api/v1/chart/0?hub=h2", &token).await,
            StatusCode::OK
        );
        assert_eq!(
            chart(&router, "/api/v1/chart/0?hub=nope", &token).await,
            StatusCode::NOT_FOUND
        );
    }

    async fn page(router: &axum::Router, uri: &str, token: &str) -> String {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        String::from_utf8(body.to_vec()).expect("utf8 html")
    }

    /// A Node series names its Hub through the dev_eui, so with two Hubs
    /// and no `?hub=` the page still charts it; only an empty store shows
    /// the waiting state (the probe page links `node:{key}:temperature`).
    #[tokio::test]
    async fn a_node_series_renders_with_two_hubs_and_no_hub_param() {
        use crate::state::events::NodeReadingMsg;

        let (state, _dir) = test_state().await;
        let token = login(&state, "admin", "correct-horse-battery").await;
        let router = super::router().with_state(state.clone());
        let uri = "/trends?s=node:aa11:temperature";

        let empty = page(&router, uri, &token).await;
        assert!(empty.contains("Waiting for device"), "{empty}");

        for hostname in ["h1", "h2"] {
            state.store.apply(IngestEvent::Status {
                hostname: hostname.into(),
                status: DeviceStatus::Online,
            });
        }
        state.store.apply(IngestEvent::NodeReading {
            hostname: "h2".into(),
            node_key: "aa11".into(),
            payload: Box::new(NodeReadingMsg {
                message_id: "m-1".into(),
                dev_eui: "aa11".into(),
                serial: None,
                fields: [("temperature".to_string(), 21.5)].into_iter().collect(),
                counters: Default::default(),
                events: Vec::new(),
                gateways: Vec::new(),
                rssi: None,
                snr: None,
                fport: None,
                page_index: None,
                page_count: None,
                dr: None,
                received_at: "2026-01-13T14:30:00Z".into(),
            }),
        });

        let html = page(&router, uri, &token).await;
        assert!(!html.contains("Waiting for device"), "{html}");
        assert!(html.contains("history-chart-config"), "{html}");
        assert!(
            html.contains("/api/v1/chart/node/aa11/temperature"),
            "{html}"
        );
        // The wired probes of both Hubs are listed directly, each token
        // naming its Hub (devices.hub.md amendment 2026-10-15).
        assert!(html.contains("line:h1:0"), "{html}");
        assert!(html.contains("line:h2:0"), "{html}");
        assert!(!html.contains("Several Hubs report here"), "{html}");
    }
}
