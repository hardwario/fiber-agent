//! `GET /alarms`, `GET /alarms/{id}`, `POST /alarms/{id}/acknowledge`,
//! `POST /alarms/{id}/notes`, `POST /alarms/acknowledge` (bulk) (M6-F).
//! All five are any-authenticated-user routes (viewer included —
//! acknowledging with a recorded reason is an operator action, not an
//! admin one, per rest.md's alarm surface).
//!
//! Context contracts: `templates/pages/alarms.html`,
//! `templates/pages/alarm_detail.html`, and `templates/partials/
//! {alarm_table,alarm_detail_body}.html` header comments. The single-alarm
//! POSTs are htmx-only and re-render `partials/alarm_detail_body.html` (the
//! `#alarm-detail-main` swap target) — a validation failure re-renders the
//! same partial with `error` set, never a 4xx/500.
//!
//! Filter default detection: the list page's filter form always submits its
//! `severity`/`alarm_type`/`show` controls, so *any* of those (or the two
//! `include_*` flags the bulk-ack round trip and older links carry) being
//! present in the query string means "a form was submitted — absent flag =
//! false". A bare `/alarms` (no such params) is a first visit and gets the
//! default view instead: `include_acknowledged = true`, `include_cleared =
//! false` (active alarms, acked or not). No hidden `filtered=1` marker
//! needed — the always-present text inputs already are one.
//!
//! `show` is the page's one lifecycle control, a select over the pair:
//! `open` (the default), `unacked` (neither flag) and `all` (both). The
//! fourth combination, cleared but not acknowledged, has no option because
//! nobody asks for it; the flags stay on the wire so the round trips and
//! `sse.rs`'s fixed view need not learn the select's vocabulary.
//!
//! Bulk acknowledge (`bulk_acknowledge` below) reuses this same filter
//! shape: `partials/alarm_table.html`'s bulk form carries the current
//! filter/pagination as hidden fields (same convention as its prev/next
//! pagination forms), so both the htmx re-render and the plain-POST
//! redirect land back on the operator's current view.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::Form as ArrayForm;
use minijinja::context;
use portal_domain::alarm_engine::ack_validation;
use portal_storage::{AlarmListFilter, AlarmSort};
use serde::Deserialize;

use crate::alarms::AlarmError;
use crate::auth::{CurrentUser, RequireManager};
use crate::http::AppState;
use crate::render;

use super::{non_empty, render as render_page, tr};

const DEFAULT_LIMIT: i64 = 50;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/alarms", get(alarms_page))
        .route("/alarms/badge", get(alarm_badge))
        .route("/alarms/acknowledge", post(bulk_acknowledge))
        .route("/alarms/{id}", get(alarm_detail_page))
        .route("/alarms/{id}/acknowledge", post(acknowledge))
        .route("/alarms/{id}/notes", post(add_note))
}

// ---------------------------------------------------------------------
// GET /alarms
// ---------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct AlarmsQuery {
    hostname: Option<String>,
    severity: Option<String>,
    alarm_type: Option<String>,
    to_state: Option<String>,
    /// The Quantity filter: a registry field name.
    field: Option<String>,
    q: Option<String>,
    include_acknowledged: Option<String>,
    include_cleared: Option<String>,
    /// `open` | `unacked` | `all` — wins over the two flags when present.
    show: Option<String>,
    /// Sort column key (`AlarmSort::key`). Anything else falls back to the
    /// list's default order rather than erroring — a hand-edited URL should
    /// show the list, not a 400.
    sort: Option<String>,
    /// `asc` | `desc`; anything else means the column's own default.
    dir: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// The `show` select's value for a flag pair, for the form and the chip.
pub(crate) fn show_value(include_acknowledged: bool, include_cleared: bool) -> &'static str {
    match (include_acknowledged, include_cleared) {
        (_, true) => "all",
        (true, false) => "open",
        (false, false) => "unacked",
    }
}

/// Query string -> storage filter, with the first-visit-vs-submitted
/// default logic documented in the module doc. Pure, for the tests below.
pub(crate) fn parse_filter(q: &AlarmsQuery) -> AlarmListFilter {
    let submitted = q.severity.is_some()
        || q.alarm_type.is_some()
        || q.to_state.is_some()
        || q.field.is_some()
        || q.show.is_some()
        || q.include_acknowledged.is_some()
        || q.include_cleared.is_some();
    let sort = q
        .sort
        .as_deref()
        .and_then(AlarmSort::from_key)
        .unwrap_or_default();
    let (include_acknowledged, include_cleared) = match q.show.as_deref() {
        Some("unacked") => (false, false),
        Some("all") => (true, true),
        Some(_) => (true, false),
        None if submitted => (
            q.include_acknowledged.is_some(),
            q.include_cleared.is_some(),
        ),
        None => (true, false),
    };
    AlarmListFilter {
        limit: q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 1000),
        offset: q.offset.unwrap_or(0).max(0),
        include_acknowledged,
        include_cleared,
        hostname: q.hostname.clone().and_then(non_empty),
        alarm_type: q.alarm_type.clone().and_then(non_empty),
        severity: q.severity.clone().and_then(non_empty),
        // `/alarms` has no per-sensor control; MB3 added these two for the
        // probe-detail window, which builds its own filter.
        sensor_id: None,
        sensor_id_prefix: None,
        sensor_field: q.field.clone().and_then(non_empty),
        to_state: q.to_state.clone().and_then(non_empty),
        search: q.q.clone().and_then(non_empty),
        search_targets: Vec::new(),
        sort,
        // Time columns read newest-first, the rest A-Z. `dir` overrides.
        sort_desc: match q.dir.as_deref() {
            Some("asc") => false,
            Some("desc") => true,
            _ => matches!(sort, AlarmSort::Start | AlarmSort::End),
        },
        // The period is /audit's; /alarms has no time bound.
        triggered_since: None,
        triggered_until: None,
    }
}

/// One "remove this filter" link per filter that is set, keyed by the query
/// parameter it drops.
///
/// The chips in the panel header are the caller's to label — the strings are
/// already in the catalogue and translating from Rust would mean plumbing the
/// language in for nothing. What a template cannot reasonably do is
/// query-string surgery: re-emitting five parameters to drop the sixth, six
/// times over, is forty lines that get one conditional wrong.
///
/// The lifecycle chip is the exception: `show` is a select, so its chip
/// puts the parameter back to `open` instead of dropping it. Dropped, a URL
/// that still carries `severity` would read as submitted with neither flag,
/// which is `unacked`, not the default.
fn chip_hrefs(filter: &AlarmListFilter) -> std::collections::BTreeMap<String, String> {
    let mut set: Vec<(&str, String)> = Vec::new();
    if let Some(v) = &filter.hostname {
        set.push(("hostname", v.clone()));
    }
    if let Some(v) = &filter.severity {
        set.push(("severity", v.clone()));
    }
    if let Some(v) = &filter.alarm_type {
        set.push(("alarm_type", v.clone()));
    }
    if let Some(v) = &filter.to_state {
        set.push(("to_state", v.clone()));
    }
    if let Some(v) = &filter.sensor_field {
        set.push(("field", v.clone()));
    }
    if let Some(v) = &filter.search {
        set.push(("q", v.clone()));
    }
    set.push((
        "show",
        show_value(filter.include_acknowledged, filter.include_cleared).to_string(),
    ));

    set.iter()
        .map(|(drop, _)| {
            let rest: Vec<String> = set
                .iter()
                .filter(|(k, _)| k != drop)
                .map(|(k, v)| format!("{}={}", k, super::percent_encode_query(v)))
                .chain((*drop == "show").then(|| "show=open".to_string()))
                .collect();
            let href = if rest.is_empty() {
                "/alarms".to_string()
            } else {
                format!("/alarms?{}", rest.join("&"))
            };
            ((*drop).to_string(), href)
        })
        .collect()
}

/// Open alarms for the badge and for the page's own header count.
///
/// `hostname` is the filter's when the caller has one; without one the count
/// spans every Hub (devices.hub.md §12 E). Until 2026-09-15 it fell back to
/// the first Hub in the store, so on a two-Hub portal the badge and the row
/// silently ignored the second one.
///
/// One function for both readers on purpose. They sit next to each other on
/// the same screen, and two copies of this derive would eventually disagree
/// about which Hub they were counting.
async fn active_alarm_count(state: &AppState, hostname: Option<String>) -> i64 {
    let snapshot = state.store.snapshot();
    let known = |hub: &str| crate::alarms::hub_known(&snapshot, hub);
    match state.alarms.active_count(hostname.as_deref(), known).await {
        Ok(count) => count,
        Err(err) => {
            tracing::error!(%err, ?hostname, "failed to count active alarms");
            0
        }
    }
}

/// The metric row's counts, over the same Hubs `active_alarm_count` counts.
async fn alarm_counts(state: &AppState, hostname: Option<String>) -> portal_storage::AlarmCounts {
    let snapshot = state.store.snapshot();
    let known = |hub: &str| crate::alarms::hub_known(&snapshot, hub);
    state
        .alarms
        .counts(hostname.as_deref(), known)
        .await
        .unwrap_or_else(|err| {
            tracing::error!(%err, ?hostname, "failed to count alarms for the metric row");
            Default::default()
        })
}

/// `GET /alarms/badge` — the sidebar pill, as its own fragment.
///
/// A fragment rather than a field on every page's context: the count needs a
/// database read and `routes::render` — the one place that could merge it
/// into every page — is synchronous. The element fetches itself on load and
/// re-fetches on the alarm nudge, which also makes it live, which a
/// render-time field would not have been.
async fn alarm_badge(_current_user: CurrentUser, State(state): State<AppState>) -> Response {
    let active_count = active_alarm_count(&state, None).await;
    crate::routes::render_fragment(
        &state,
        "partials/nav_alarm_badge.html",
        context! { active_count => active_count },
    )
}

async fn alarms_page(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Query(q): Query<AlarmsQuery>,
) -> Response {
    let filter = parse_filter(&q);
    let filter_ctx = context! {
        hostname => filter.hostname.clone(),
        severity => filter.severity.clone(),
        alarm_type => filter.alarm_type.clone(),
        to_state => filter.to_state.clone(),
        field => filter.sensor_field.clone(),
        q => filter.search.clone(),
        include_acknowledged => filter.include_acknowledged,
        include_cleared => filter.include_cleared,
        show => show_value(filter.include_acknowledged, filter.include_cleared),
    };
    let (limit, offset) = (filter.limit, filter.offset);
    let (sort, sort_desc) = (filter.sort, filter.sort_desc);
    // Built before `filter` is handed to `list`, which takes it by value.
    let chip_hrefs = chip_hrefs(&filter);
    // Everything a sort header has to carry over except `sort`/`dir`, which
    // it sets itself, and `offset`: a re-sorted list starts at page one.
    // Did the operator narrow the list? Decides which of the two empty
    // states the table shows — "nothing matches" is a different fact from
    // "no alarm has ever fired", and one message for both sent people
    // looking for a bug that was a leftover filter.
    let filtered = filter.hostname.is_some()
        || filter.severity.is_some()
        || filter.alarm_type.is_some()
        || filter.to_state.is_some()
        || filter.sensor_field.is_some()
        || filter.search.is_some()
        || !filter.include_cleared
        || !filter.include_acknowledged;
    let sort_qs = super::filter_qs(&[
        ("hostname", filter.hostname.clone()),
        ("severity", filter.severity.clone()),
        ("alarm_type", filter.alarm_type.clone()),
        ("to_state", filter.to_state.clone()),
        ("field", filter.sensor_field.clone()),
        ("q", filter.search.clone()),
        (
            "show",
            Some(show_value(filter.include_acknowledged, filter.include_cleared).to_string()),
        ),
    ]);

    // The device dropdown's options. Sourced from the state store rather than
    // `SELECT DISTINCT hostname FROM alarm_events`, so it needs no new storage
    // method — the cost is that a device with history but no live presence
    // (never seen since boot) is filterable by URL but not offered here.
    let mut devices: Vec<String> = state
        .store
        .snapshot()
        .devices
        .values()
        .map(|d| d.hostname.clone())
        .collect();
    devices.sort();
    devices.dedup();

    let active_count = active_alarm_count(&state, filter.hostname.clone()).await;
    // The metric row, built here rather than in `routes::render`: these four
    // numbers are rows of the `alarm_events` log, which takes a database
    // read. `render` leaves a `stats` the page already carries alone.
    let stats = render::alarm_stats_ctx(
        state.store.snapshot_mqtt().connected,
        alarm_counts(&state, filter.hostname.clone()).await,
    );

    // The search runs in SQL over the stored names, which are the registry's English ones,
    // while a Czech page shows them translated (N25, night test): "Teplota" finds what the page
    // calls Teplota. Only the query goes to storage translated; the form keeps what was typed.
    let mut filter = filter;
    if let Some(q) = filter.search.as_deref() {
        filter.search_targets = search_targets(&state.store.snapshot(), q);
    }
    if let Some(source) = filter
        .search
        .as_deref()
        .and_then(|q| crate::i18n::field_label_source(crate::i18n::current_lang(), q))
    {
        filter.search = Some(source);
    }

    match state.alarms.list(filter).await {
        Ok((alarms, total)) => {
            let rows = list_rows(&state, &alarms).await;
            let ctx = render::page_ctx(
                context! {
                    nav_active => "alarms",
                    stats => stats,
                    alarms => rows,
                    total => total,
                    limit => limit,
                    offset => offset,
                    filter => filter_ctx,
                    devices => devices,
                    active_count => active_count,
                    chip_hrefs => chip_hrefs,
                    filtered => filtered,
                    sort => sort.key(),
                    dir => if sort_desc { "desc" } else { "asc" },
                    sort_qs => sort_qs,
                },
                Some(&current_user),
            );
            render_page(&state, "pages/alarms.html", ctx)
        }
        Err(err) => {
            tracing::error!(%err, "failed to list alarms");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to load alarms").into_response()
        }
    }
}

/// The `sensor_id` prefixes whose device the live registry names, places
/// or numbers the way `q` reads (case-insensitive substring). The SQL search
/// covers the stored columns and `device_meta`; a name a Node or Beacon
/// reports itself, or a wired probe's name, lives only here.
pub(crate) fn search_targets(state: &crate::state::PortalState, q: &str) -> Vec<(String, String)> {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let hit = |parts: &[Option<&str>]| {
        parts
            .iter()
            .flatten()
            .any(|p| p.to_lowercase().contains(&q))
    };
    let mut out = Vec::new();
    for d in state.devices.values() {
        for (key, n) in &d.nodes {
            if hit(&[
                n.name.as_deref(),
                n.location.as_deref(),
                n.serial_number.as_deref(),
            ]) {
                out.push((String::new(), format!("lrw:{key}:")));
                out.push((String::new(), format!("p2p:{key}:")));
                out.push((String::new(), format!("cal:node:{key}")));
            }
        }
        for (mac, b) in &d.ble_sensors {
            if hit(&[b.name.as_deref(), b.location.as_deref()]) {
                out.push((String::new(), format!("eye:{mac}:")));
                out.push((String::new(), format!("cal:eye:{mac}")));
            }
        }
        for s in &d.sensors {
            let line = s.line;
            if hit(&[s.name.as_deref(), s.location.as_deref()]) {
                out.push((d.hostname.clone(), format!("line-{line}")));
                out.push((d.hostname.clone(), format!("cal:probe:{line}")));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

// ---------------------------------------------------------------------
// GET /alarms/{id}
// ---------------------------------------------------------------------

/// A missing (pruned, or mistyped) alarm: the 404 page inside the layout,
/// saying it in the reader's words (UX-026). The two POSTs answer the same;
/// htmx does not swap a 404, so the page never lands in a fragment slot.
fn alarm_not_found(state: &AppState, current_user: &CurrentUser) -> Response {
    super::not_found_saying(state, Some(current_user), tr("alarms.detail.not_found"))
}

async fn alarm_detail_page(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    match state.alarms.detail(&id).await {
        Ok(Some((alarm, notes))) => {
            let names = state
                .db
                .full_names(render::alarm_detail_usernames(&alarm, &notes))
                .await;
            let ctx = render::page_ctx(
                context! {
                    nav_active => "alarms",
                    ..render::alarm_detail_body_ctx(
                        &state.store.snapshot(),
                        &alarm,
                        &notes,
                        &names,
                        alarm_peak(&state, &alarm).await,
                        None
                    )
                },
                Some(&current_user),
            );
            render_page(&state, "pages/alarm_detail.html", ctx)
        }
        Ok(None) => alarm_not_found(&state, &current_user),
        Err(err) => {
            tracing::error!(%err, alarm_id = %id, "failed to load alarm detail");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to load alarm").into_response()
        }
    }
}

/// The alarm's peak over its window, `[triggered_at, cleared_at or now]`
/// (decisions.md #24): the highest value for a HIGH alarm, the lowest for a
/// LOW one, over the probe's stored readings **and** the value the alarm row
/// itself carries. Both are needed: the row only sees its transitions, and a
/// stored minute average flattens a spike the transition sampled. The time
/// is known only when a stored reading is the peak. `None` for an alarm with
/// no quantity, and for any alarm but HIGH and LOW: a DISCONNECT on a wired
/// line has a quantity but no value it went past, so a "highest value" over
/// its window would answer a question nobody asked.
async fn alarm_peak(state: &AppState, a: &portal_storage::AlarmEvent) -> Option<render::AlarmPeak> {
    alarm_peaks(state, std::slice::from_ref(a))
        .await
        .pop()
        .flatten()
}

/// A list page's rows, each with its peak beside the worst-severity badge
/// (UX-025), derived as the detail derives it.
async fn list_rows(
    state: &AppState,
    alarms: &[portal_storage::AlarmEvent],
) -> Vec<render::AlarmCtx> {
    let snapshot = state.store.snapshot();
    let peaks = alarm_peaks(state, alarms).await;
    alarms
        .iter()
        .zip(peaks)
        .map(|(a, peak)| render::AlarmCtx {
            peak: peak.map(|p| render::alarm_shown(a, p.value)),
            ..render::alarm_ctx(a, &snapshot)
        })
        .collect()
}

/// [`alarm_peak`] for every row of a list page (UX-025), in one trip to the
/// storage thread rather than one per row.
///
/// Each query scans and sorts the alarm's whole window, which for a
/// week-long alarm is thousands of readings, and a page has up to its limit
/// of rows. An ended alarm's window no longer grows, so its peak is taken
/// from [`crate::memo::RenderMemos::alarm_peaks`] when it is there; only open
/// alarms and ended ones not seen for a while reach the database.
async fn alarm_peaks(
    state: &AppState,
    alarms: &[portal_storage::AlarmEvent],
) -> Vec<Option<render::AlarmPeak>> {
    let memo = &state.memos.alarm_peaks;
    let cached: Vec<Option<Option<render::AlarmPeak>>> = alarms
        .iter()
        .map(|a| a.cleared_at.as_ref().and_then(|_| memo.get(&a.id)))
        .collect();
    let missing: Vec<portal_storage::AlarmEvent> = alarms
        .iter()
        .zip(&cached)
        .filter(|(_, hit)| hit.is_none())
        .map(|(a, _)| a.clone())
        .collect();
    // `None` when the storage trip failed: those rows go without a peak, and
    // nothing is memoised from the failure.
    let computed = match missing.is_empty() {
        true => Some(Vec::new()),
        false => {
            let now = render::now_unix();
            state
                .db
                .run(move |s| Ok(missing.iter().map(|a| peak_on(s, a, now)).collect()))
                .await
                .ok()
        }
    };
    let remember = computed.is_some();
    let mut computed = computed.unwrap_or_default().into_iter();
    alarms
        .iter()
        .zip(cached)
        .map(|(a, hit)| {
            hit.unwrap_or_else(|| {
                let peak = computed.next().flatten();
                if remember && a.cleared_at.is_some() {
                    memo.insert(a.id.clone(), peak);
                }
                peak
            })
        })
        .collect()
}

/// [`alarm_peak`]'s work for one row, on the storage thread.
fn peak_on(
    s: &portal_storage::Storage,
    a: &portal_storage::AlarmEvent,
    now: i64,
) -> Option<render::AlarmPeak> {
    use crate::alarms::AlarmTarget;
    use portal_storage::{EyeField, PeakSource};
    let highest = match a.alarm_type.as_str() {
        "HIGH" => true,
        "LOW" => false,
        _ => return None,
    };
    let field = crate::alarms::alarm_field(&a.sensor_id, a.sensor_name.as_deref())?;
    let unix = |iso: &str| {
        time::OffsetDateTime::parse(iso, &time::format_description::well_known::Rfc3339)
            .ok()
            .map(|t| t.unix_timestamp())
    };
    let from = unix(&a.triggered_at)?;
    // +1: the window is half-open and the clearing reading belongs to it.
    let to = a.cleared_at.as_deref().and_then(unix).unwrap_or(now) + 1;
    let target = crate::alarms::alarm_target(&a.sensor_id);
    // The wired tier stores minute averages only (`readings_minute.avg_c`).
    let wired = matches!(target, AlarmTarget::Line(_));
    let source = match &target {
        AlarmTarget::Line(line) => Some(PeakSource::Wired { line: *line }),
        AlarmTarget::Node(key) => Some(PeakSource::Node {
            node_key: key,
            field: field.name,
        }),
        AlarmTarget::Beacon(mac) => {
            EyeField::from_field_name(field.name).map(|field| PeakSource::Eye { mac, field })
        }
        AlarmTarget::Hub => None,
    };
    let stored = match source {
        Some(source) => s
            .peak_in_window(&a.hostname, source, from, to, highest)
            .unwrap_or_else(|err| {
                tracing::warn!(%err, alarm_id = %a.id, "alarm peak query failed; shown without it");
                None
            }),
        None => None,
    };
    let (value, at) = combine_peak(stored, a.value, highest)?;
    Some(render::AlarmPeak {
        value,
        at,
        minute_average: wired && at.is_some(),
    })
}

/// The stored peak `(value, ts)` against the row's own value: whichever is
/// further out wins, and only a stored reading brings a time.
fn combine_peak(
    stored: Option<(f64, i64)>,
    row: Option<f64>,
    highest: bool,
) -> Option<(f64, Option<i64>)> {
    let beyond = |v: f64, than: f64| if highest { v > than } else { v < than };
    match (stored, row) {
        (Some((v, ts)), Some(row)) if !beyond(row, v) => Some((v, Some(ts))),
        (_, Some(row)) => Some((row, None)),
        (Some((v, ts)), None) => Some((v, Some(ts))),
        (None, None) => None,
    }
}

/// Re-renders `partials/alarm_detail_body.html` (the htmx swap response for
/// both POSTs below), optionally with a validation `error` banner.
async fn render_detail_body(
    state: &AppState,
    current_user: &CurrentUser,
    id: &str,
    error: Option<String>,
) -> Response {
    match state.alarms.detail(id).await {
        Ok(Some((alarm, notes))) => {
            let names = state
                .db
                .full_names(render::alarm_detail_usernames(&alarm, &notes))
                .await;
            let ctx = render::page_ctx(
                render::alarm_detail_body_ctx(
                    &state.store.snapshot(),
                    &alarm,
                    &notes,
                    &names,
                    alarm_peak(state, &alarm).await,
                    error,
                ),
                Some(current_user),
            );
            render_page(state, "partials/alarm_detail_body.html", ctx)
        }
        Ok(None) => alarm_not_found(state, current_user),
        Err(err) => {
            tracing::error!(%err, alarm_id = %id, "failed to re-render alarm detail");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to load alarm").into_response()
        }
    }
}

// ---------------------------------------------------------------------
// POST /alarms/{id}/acknowledge
// ---------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct AckForm {
    #[serde(default)]
    reason: String,
}

async fn acknowledge(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<AckForm>,
) -> Response {
    match state
        .alarms
        .acknowledge(&id, &current_user, &form.reason)
        .await
    {
        Ok(_) => {
            state
                .auth
                .audit(
                    "alarm_ack",
                    Some(&current_user),
                    None,
                    Some(&id),
                    None,
                    true,
                )
                .await;
            render_detail_body(&state, &current_user, &id, None).await
        }
        Err(AlarmError::InvalidText(problem)) => {
            let key = format!("alarms.errors.{}", problem.slug());
            render_detail_body(&state, &current_user, &id, Some(key)).await
        }
        Err(AlarmError::NotFound) => alarm_not_found(&state, &current_user),
        Err(err) => {
            tracing::error!(%err, alarm_id = %id, "failed to acknowledge alarm");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to acknowledge alarm",
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------
// POST /alarms/acknowledge (bulk)
// ---------------------------------------------------------------------

/// `partials/alarm_table.html`'s bulk-ack form: `ids[]` is a repeated
/// checkbox value, which plain `axum::Form` (serde_urlencoded) can't
/// deserialize into a `Vec` — hence `axum_extra::extract::Form` (aliased
/// `ArrayForm` above), which does. The rest of the fields are the same
/// filter/pagination shape `AlarmsQuery` carries, mirrored here (rather
/// than reused directly) because they arrive as hidden form fields, not a
/// query string.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BulkAckForm {
    #[serde(rename = "ids[]")]
    ids: Vec<String>,
    reason: String,
    hostname: Option<String>,
    severity: Option<String>,
    alarm_type: Option<String>,
    to_state: Option<String>,
    field: Option<String>,
    /// The free-text search, carried through the bulk form's hidden fields
    /// like every other filter: without it, acknowledging a selection would
    /// return to an unsearched list. Same reason `to_state` is here.
    q: Option<String>,
    include_acknowledged: Option<String>,
    include_cleared: Option<String>,
    /// Carried so acknowledging a batch returns to the list the operator was
    /// looking at, sort included.
    sort: Option<String>,
    dir: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

impl BulkAckForm {
    fn as_query(&self) -> AlarmsQuery {
        AlarmsQuery {
            hostname: self.hostname.clone(),
            severity: self.severity.clone(),
            alarm_type: self.alarm_type.clone(),
            to_state: self.to_state.clone(),
            field: self.field.clone(),
            q: self.q.clone(),
            sort: self.sort.clone(),
            dir: self.dir.clone(),
            include_acknowledged: self.include_acknowledged.clone(),
            include_cleared: self.include_cleared.clone(),
            show: None,
            limit: self.limit,
            offset: self.offset,
        }
    }
}

/// Minimal percent-encoding for one query-string value: the RFC 3986
/// unreserved set stays literal, everything else (including space) becomes
/// `%XX` per UTF-8 byte. `alarm_type`/`hostname` are free text, so this
/// route is the one alarm_table.html's header comment notes has no
/// `urlencode` filter to reach for — a handful of values doesn't earn a new
/// dependency (`form_urlencoded` is already resolved transitively via
/// axum-extra's `form` feature, just not a direct dependency of this
/// crate).
fn encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The bulk form's hidden filter/pagination fields, round-tripped onto the
/// plain-POST redirect target exactly as `parse_filter` normalized them —
/// `severity`/`alarm_type` are always present (even blank) so the "a form
/// was submitted" marker this module's doc comment describes survives the
/// redirect too.
fn redirect_query_string(filter: &AlarmListFilter) -> String {
    let mut parts = vec![
        format!(
            "hostname={}",
            encode_query_value(filter.hostname.as_deref().unwrap_or(""))
        ),
        format!(
            "severity={}",
            encode_query_value(filter.severity.as_deref().unwrap_or(""))
        ),
        format!(
            "alarm_type={}",
            encode_query_value(filter.alarm_type.as_deref().unwrap_or(""))
        ),
        format!("limit={}", filter.limit),
        format!("offset={}", filter.offset),
        format!("sort={}", filter.sort.key()),
        format!("dir={}", if filter.sort_desc { "desc" } else { "asc" }),
    ];
    if filter.include_acknowledged {
        parts.push("include_acknowledged=true".to_string());
    }
    if filter.include_cleared {
        parts.push("include_cleared=true".to_string());
    }
    parts.join("&")
}

/// Shared tail of `bulk_acknowledge`: re-renders `partials/alarm_table.html`
/// for an htmx request (same context contract as `alarms_page`, plus
/// `bulk_summary`/`bulk_error`), or redirects back to `/alarms` with the
/// filter preserved for a plain POST (no-JS fallback).
async fn respond_to_bulk_ack(
    state: &AppState,
    current_user: &CurrentUser,
    headers: &HeaderMap,
    query: &AlarmsQuery,
    summary: Option<String>,
    error_key: Option<String>,
) -> Response {
    let filter = parse_filter(query);

    if !headers.contains_key("HX-Request") {
        return Redirect::to(&format!("/alarms?{}", redirect_query_string(&filter)))
            .into_response();
    }

    let filter_ctx = context! {
        hostname => filter.hostname.clone(),
        severity => filter.severity.clone(),
        alarm_type => filter.alarm_type.clone(),
        include_acknowledged => filter.include_acknowledged,
        include_cleared => filter.include_cleared,
    };
    let (limit, offset) = (filter.limit, filter.offset);
    let (sort, sort_desc) = (filter.sort, filter.sort_desc);
    // The metric row goes back out of band: the acknowledged rows just left
    // "Nepotvrzené", and the tiles are not on the SSE stats refresh.
    let stats = render::alarm_stats_ctx(
        state.store.snapshot_mqtt().connected,
        alarm_counts(state, filter.hostname.clone()).await,
    );

    match state.alarms.list(filter).await {
        Ok((alarms, total)) => {
            let rows = list_rows(state, &alarms).await;
            let ctx = render::page_ctx(
                context! {
                    stats => stats,
                    stats_oob => true,
                    alarms => rows,
                    total => total,
                    limit => limit,
                    offset => offset,
                    filter => filter_ctx,
                    bulk_summary => summary,
                    bulk_error => error_key,
                    // The swapped-in table keeps the order it was sorted by;
                    // its headers and page links read these back.
                    sort => sort.key(),
                    dir => if sort_desc { "desc" } else { "asc" },
                    sort_qs => String::new(),
                    // The page instance stays out of the fixed-filter SSE
                    // re-render — see alarm_table.html's `sse_live` note.
                    sse_live => false,
                },
                Some(current_user),
            );
            render_page(state, "partials/alarm_table.html", ctx)
        }
        Err(err) => {
            tracing::error!(%err, "failed to list alarms after bulk acknowledge");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to load alarms").into_response()
        }
    }
}

/// Acknowledges every id in `ids[]` with the same `reason`, going through
/// the exact same `AlarmService::acknowledge` path the single-alarm route
/// uses (one audit row per alarm). An id that's already acknowledged or
/// already cleared — or doesn't exist — is skipped and counted, not an
/// error; only a blank/too-long reason fails the whole request, mirroring
/// the single-ack route's `ack_with_blank_reason_renders_error_not_500`
/// behaviour (a 200 re-render with the error, never a 4xx/500).
async fn bulk_acknowledge(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    headers: HeaderMap,
    ArrayForm(form): ArrayForm<BulkAckForm>,
) -> Response {
    let query = form.as_query();

    if let Err(problem) = ack_validation(&form.reason) {
        let key = format!("alarms.errors.{}", problem.slug());
        return respond_to_bulk_ack(&state, &current_user, &headers, &query, None, Some(key)).await;
    }

    let mut acked = 0i64;
    let mut skipped = 0i64;
    for id in &form.ids {
        match state.alarms.detail(id).await {
            Ok(Some((alarm, _))) if alarm.acknowledged_at.is_none() => {
                match state
                    .alarms
                    .acknowledge(id, &current_user, &form.reason)
                    .await
                {
                    Ok(_) => {
                        state
                            .auth
                            .audit("alarm_ack", Some(&current_user), None, Some(id), None, true)
                            .await;
                        acked += 1;
                    }
                    Err(AlarmError::NotFound) => skipped += 1,
                    Err(err) => {
                        tracing::error!(%err, alarm_id = %id, "failed to bulk-acknowledge alarm");
                        skipped += 1;
                    }
                }
            }
            Ok(_) => skipped += 1, // already acknowledged, or not found
            Err(err) => {
                tracing::error!(%err, alarm_id = %id, "failed to load alarm for bulk acknowledge");
                skipped += 1;
            }
        }
    }

    let summary = tr("alarms.bulk.summary")
        .replace("{acked}", &acked.to_string())
        .replace("{skipped}", &skipped.to_string());
    respond_to_bulk_ack(&state, &current_user, &headers, &query, Some(summary), None).await
}

// ---------------------------------------------------------------------
// POST /alarms/{id}/notes
// ---------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct NoteForm {
    #[serde(default)]
    content: String,
}

/// Route-side content bound (1..=2000 chars). Blankness is re-checked by
/// the service too; the upper bound is this route's own contract (the
/// textarea's `maxlength`, re-validated server-side). Returns an
/// `alarms.errors.*` catalogue key (M9), not a message — `partials/
/// alarm_detail_body.html` renders `error` through the `t` filter. Every
/// value that can reach `error` is such a key: this route's own checks and
/// `AlarmError::InvalidText`'s slugs alike.
fn validate_note_content(content: &str) -> Result<(), String> {
    if content.trim().is_empty() || content.chars().count() > 2000 {
        return Err("alarms.errors.note_length".to_string());
    }
    Ok(())
}

async fn add_note(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<NoteForm>,
) -> Response {
    if let Err(msg) = validate_note_content(&form.content) {
        return render_detail_body(&state, &current_user, &id, Some(msg)).await;
    }
    match state
        .alarms
        .add_note(&id, &current_user, &form.content)
        .await
    {
        Ok(_) => {
            // HS-11 / LE-13 (night tests 2026-09-26): a note left no trace in the audit,
            // unlike the acknowledge it often accompanies. The length, not the text: the text
            // is on the alarm itself.
            let details = serde_json::json!({ "length": form.content.chars().count() }).to_string();
            state
                .auth
                .audit(
                    "alarm_note",
                    Some(&current_user),
                    None,
                    Some(&id),
                    Some(details),
                    true,
                )
                .await;
            render_detail_body(&state, &current_user, &id, None).await
        }
        Err(AlarmError::InvalidText(problem)) => {
            let key = format!("alarms.errors.{}", problem.slug());
            render_detail_body(&state, &current_user, &id, Some(key)).await
        }
        Err(AlarmError::NotFound) => alarm_not_found(&state, &current_user),
        Err(err) => {
            tracing::error!(%err, alarm_id = %id, "failed to add alarm note");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to add note").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seen live on 2026-09-25: minute averages of ~10 °C against a
    /// transition that sampled 52.8 °C. The peak is the transition's value,
    /// without a time; a stored reading further out wins with its time.
    #[test]
    fn the_peak_is_the_further_of_stored_and_row() {
        assert_eq!(
            combine_peak(Some((10.0, 60)), Some(52.8), true),
            Some((52.8, None))
        );
        assert_eq!(
            combine_peak(Some((55.0, 60)), Some(52.8), true),
            Some((55.0, Some(60)))
        );
        assert_eq!(
            combine_peak(Some((-3.0, 60)), Some(-1.0), false),
            Some((-3.0, Some(60)))
        );
        assert_eq!(combine_peak(None, Some(-1.0), false), Some((-1.0, None)));
        assert_eq!(combine_peak(None, None, true), None);
    }

    fn query(
        severity: Option<&str>,
        alarm_type: Option<&str>,
        include_acknowledged: Option<&str>,
        include_cleared: Option<&str>,
    ) -> AlarmsQuery {
        AlarmsQuery {
            hostname: None,
            severity: severity.map(str::to_string),
            alarm_type: alarm_type.map(str::to_string),
            to_state: None,
            field: None,
            q: None,
            include_acknowledged: include_acknowledged.map(str::to_string),
            include_cleared: include_cleared.map(str::to_string),
            show: None,
            sort: None,
            dir: None,
            limit: None,
            offset: None,
        }
    }

    #[test]
    fn first_visit_defaults_to_acked_yes_cleared_no() {
        // Bare `/alarms` — no filter params at all.
        let filter = parse_filter(&query(None, None, None, None));
        assert!(filter.include_acknowledged, "first visit shows acked");
        assert!(!filter.include_cleared, "first visit hides cleared");
    }

    #[test]
    fn submitted_form_with_unchecked_boxes_means_false() {
        // The form always submits severity/alarm_type (even empty), so
        // their presence marks a submission — absent checkboxes are FALSE
        // now, not the first-visit default.
        let filter = parse_filter(&query(Some(""), Some(""), None, None));
        assert!(!filter.include_acknowledged);
        assert!(!filter.include_cleared);
    }

    #[test]
    fn submitted_form_with_checked_boxes_means_true() {
        let filter = parse_filter(&query(Some(""), Some(""), Some("true"), Some("true")));
        assert!(filter.include_acknowledged);
        assert!(filter.include_cleared);
    }

    #[test]
    fn checkbox_alone_also_counts_as_submission() {
        // Defensive: a hand-built URL with only a checkbox param.
        let filter = parse_filter(&query(None, None, None, Some("true")));
        assert!(!filter.include_acknowledged);
        assert!(filter.include_cleared);
    }

    #[test]
    fn blank_text_filters_become_none_and_values_pass_through() {
        let q = AlarmsQuery {
            hostname: Some("  ".into()),
            severity: Some("critical".into()),
            alarm_type: Some(" DISCONNECT ".into()),
            to_state: Some("  ".into()),
            field: None,
            q: Some("  Zone G  ".into()),
            include_acknowledged: None,
            include_cleared: None,
            show: None,
            sort: None,
            dir: None,
            limit: Some(5000),
            offset: Some(-3),
        };
        let filter = parse_filter(&q);
        assert_eq!(filter.hostname, None);
        assert_eq!(filter.severity.as_deref(), Some("critical"));
        assert_eq!(filter.alarm_type.as_deref(), Some("DISCONNECT"));
        assert_eq!(filter.to_state, None);
        assert_eq!(
            filter.search.as_deref(),
            Some("Zone G"),
            "the search is trimmed like every other text filter, and reaches \
             the SQL clause — a client-side pass would only ever have seen \
             the rows of the current page"
        );
        assert_eq!(filter.limit, 1000, "clamped");
        assert_eq!(filter.offset, 0, "negative offset clamps to 0");
    }

    #[test]
    fn note_content_bounds() {
        assert!(validate_note_content("looks fine").is_ok());
        assert!(validate_note_content("   ").is_err());
        assert!(validate_note_content(&"x".repeat(2000)).is_ok());
        assert!(validate_note_content(&"x".repeat(2001)).is_err());
    }

    mod integration {
        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use tower::ServiceExt;

        use crate::auth::SESSION_COOKIE;
        use crate::routes::test_support::{login, test_state};

        /// Inserts one open alarm straight into the test dir's `portal.db`
        /// (another WAL connection, same as every service struct) and
        /// returns its id.
        fn seed_alarm(dir: &std::path::Path) -> String {
            let storage =
                portal_storage::Storage::open(&dir.join("portal.db"), None).expect("open storage");
            storage
                .insert_alarm_event(&portal_storage::NewAlarmEvent {
                    hostname: "h1".into(),
                    sensor_id: "line-0".into(),
                    sensor_name: Some("Fridge".into()),
                    sensor_location: Some("kitchen".into()),
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

        async fn send(
            router: &axum::Router,
            method: &str,
            path: &str,
            token: Option<&str>,
            form_body: Option<&str>,
        ) -> axum::http::Response<Body> {
            let mut builder = Request::builder().method(method).uri(path);
            if let Some(token) = token {
                builder = builder.header(header::COOKIE, format!("{SESSION_COOKIE}={token}"));
            }
            let request = match form_body {
                Some(body) => builder
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body.to_string()))
                    .expect("request"),
                None => builder.body(Body::empty()).expect("request"),
            };
            router.clone().oneshot(request).await.expect("response")
        }

        async fn body_string(response: axum::http::Response<Body>) -> String {
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            String::from_utf8(bytes.to_vec()).expect("utf8 body")
        }

        /// Same as `send`, plus the `HX-Request` header the bulk-ack route
        /// branches on (htmx re-render vs. plain-POST redirect).
        async fn send_htmx(
            router: &axum::Router,
            path: &str,
            token: &str,
            form_body: &str,
        ) -> axum::http::Response<Body> {
            let request = Request::builder()
                .method("POST")
                .uri(path)
                .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header("HX-Request", "true")
                .body(Body::from(form_body.to_string()))
                .expect("request");
            router.clone().oneshot(request).await.expect("response")
        }

        /// N21 (night test): the detail's Field box said "Temperature" for every alarm, a v1
        /// leftover from the wired probes. A radio alarm names its own quantity, in the page's
        /// language through `field_label`.
        #[tokio::test]
        async fn a_radio_alarm_names_its_own_quantity() {
            let (state, dir) = test_state().await;
            let storage =
                portal_storage::Storage::open(&dir.path().join("portal.db"), None).expect("open");
            let id = storage
                .insert_alarm_event(&portal_storage::NewAlarmEvent {
                    hostname: "h1".into(),
                    sensor_id: "lrw:5876070000000413:Humidity".into(),
                    sensor_name: Some("Humidity".into()),
                    sensor_location: None,
                    alarm_type: "HIGH".into(),
                    severity: "critical".into(),
                    from_state: Some("NORMAL".into()),
                    to_state: Some("CRITICAL".into()),
                    message: None,
                    value: Some(64.0),
                    threshold: Some(60.0),
                    triggered_at: "2026-09-24T18:31:53Z".into(),
                    cleared_at: None,
                    acknowledged_at: None,
                    acknowledged_by_user_id: None,
                    acknowledged_by_username: None,
                    native_key: None,
                })
                .expect("seed alarm");
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state);

            let html = body_string(
                send(&router, "GET", &format!("/alarms/{id}"), Some(&token), None).await,
            )
            .await;
            let field_box = html
                .split("Channel</div>")
                .nth(1)
                .and_then(|rest| rest.split("</div>").next())
                .unwrap_or_default();
            assert!(field_box.contains("Humidity"), "{field_box}");
            assert!(!field_box.contains("Temperature"), "{field_box}");
        }

        #[tokio::test]
        async fn anonymous_requests_redirect_to_login() {
            let (state, _dir) = test_state().await;
            let router = super::super::router().with_state(state);

            let response = send(&router, "GET", "/alarms", None, None).await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            assert_eq!(
                response.headers().get(header::LOCATION).unwrap(),
                "/login?next=%2Falarms"
            );
            // An alarm e-mail's link survives the login (UX-027).
            let response = send(&router, "GET", "/alarms/8921fdc5-aa", None, None).await;
            assert_eq!(
                response.headers().get(header::LOCATION).unwrap(),
                "/login?next=%2Falarms%2F8921fdc5-aa"
            );
        }

        #[tokio::test]
        async fn ack_with_blank_reason_renders_error_not_500() {
            let (state, dir) = test_state().await;
            let id = seed_alarm(dir.path());
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state);

            let response = send(
                &router,
                "POST",
                &format!("/alarms/{id}/acknowledge"),
                Some(&token),
                Some("reason=+++"),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "error re-render, not a 4xx/500"
            );
            let html = body_string(response).await;
            assert!(
                html.contains("alert-error"),
                "error banner present:\n{html}"
            );
            assert!(
                html.contains("id=\"alarm-detail-main\""),
                "swap target re-rendered"
            );
        }

        #[tokio::test]
        async fn ack_with_reason_succeeds_and_rerenders_acknowledged() {
            let (state, dir) = test_state().await;
            let id = seed_alarm(dir.path());
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state);

            let response = send(
                &router,
                "POST",
                &format!("/alarms/{id}/acknowledge"),
                Some(&token),
                Some("reason=confirmed+on-site"),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let html = body_string(response).await;
            assert!(html.contains("Acknowledged"), "ack badge rendered:\n{html}");
            assert!(
                html.contains("confirmed on-site"),
                "reason stored as a note"
            );
            assert!(!html.contains("alert-error"));
        }

        /// A wired alarm of `alarm_type` carrying `value`, plus one stored
        /// minute averaging `stored_avg` inside its window, when given.
        fn seed_wired(
            dir: &std::path::Path,
            alarm_type: &str,
            value: Option<f64>,
            stored_avg: Option<f64>,
        ) -> String {
            let storage =
                portal_storage::Storage::open(&dir.join("portal.db"), None).expect("open storage");
            let id = storage
                .insert_alarm_event(&portal_storage::NewAlarmEvent {
                    hostname: "h1".into(),
                    sensor_id: "line-0".into(),
                    sensor_name: Some("Fridge".into()),
                    sensor_location: None,
                    alarm_type: alarm_type.into(),
                    severity: "warning".into(),
                    from_state: Some("NORMAL".into()),
                    to_state: Some("WARNING".into()),
                    message: None,
                    value,
                    threshold: Some(8.0),
                    triggered_at: "2026-08-18T00:00:00Z".into(),
                    cleared_at: None,
                    acknowledged_at: None,
                    acknowledged_by_user_id: None,
                    acknowledged_by_username: None,
                    native_key: None,
                })
                .expect("seed alarm");
            if let Some(avg) = stored_avg {
                storage
                    .upsert_minute(&portal_storage::MinuteReading {
                        hostname: "h1".into(),
                        sensor_line: 0,
                        // One minute after `seed_alarm`'s triggered_at.
                        minute_ts: 1_787_011_260,
                        min_c: Some(avg),
                        avg_c: Some(avg),
                        max_c: Some(avg),
                        sample_count: Some(6),
                        disconnect_count: Some(0),
                        worst_alarm: None,
                        source: "live".into(),
                        data_hmac: None,
                    })
                    .expect("minute");
            }
            id
        }

        /// UX-025: the list shows the peak beside the worst-severity badge,
        /// labelled as the highest, derived as the detail derives it: the
        /// stored minute (12.0) is beyond the row's own value (9.0).
        #[tokio::test]
        async fn the_list_shows_the_peak_beside_the_severity_badge() {
            let (state, dir) = test_state().await;
            seed_wired(dir.path(), "HIGH", Some(9.0), Some(12.0));
            seed_wired(dir.path(), "DISCONNECT", None, Some(12.0));
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state);

            let list = body_string(send(&router, "GET", "/alarms", Some(&token), None).await).await;
            assert_eq!(
                list.matches("Highest 12.0").count(),
                1,
                "one peak, for the value alarm only:\n{list}"
            );
        }

        /// #111: a LOW alarm's peak is its minimum, and the list names it
        /// "Lowest" as the detail says "Lowest value" — not "Peak", and not
        /// the "Highest" a HIGH row beside it wears. Both rows sit on line 0
        /// and so share its one stored minute (6.0): the LOW row's peak is
        /// that minute, the HIGH row's is its own value (9.0).
        #[tokio::test]
        async fn the_list_names_a_low_alarms_peak_lowest() {
            let (state, dir) = test_state().await;
            seed_wired(dir.path(), "HIGH", Some(9.0), None);
            seed_wired(dir.path(), "LOW", Some(7.5), Some(6.0));
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state);

            let list = body_string(send(&router, "GET", "/alarms", Some(&token), None).await).await;
            assert_eq!(list.matches("Lowest 6.0").count(), 1, "{list}");
            assert_eq!(list.matches("Highest 9.0").count(), 1, "{list}");
            assert!(!list.contains("Highest 6.0"), "{list}");
            assert!(!list.contains("Peak"), "{list}");
        }

        /// MR !103 review: an ended alarm's peak is memoised, so a list
        /// render does not scan its window again. A reading stored in the
        /// window afterwards (a late backfill) shows once the memo expires;
        /// an open alarm's peak is always read live.
        #[tokio::test]
        async fn an_ended_alarms_peak_is_memoised_and_an_open_ones_is_not() {
            let (state, dir) = test_state().await;
            let ended = seed_wired(dir.path(), "HIGH", Some(9.0), Some(12.0));
            let storage = portal_storage::Storage::open(&dir.path().join("portal.db"), None)
                .expect("open storage");
            storage
                .clear_alarm(&ended, "2026-08-18T01:00:00Z", None)
                .expect("clear");
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state.clone());
            let list = || async {
                let path = "/alarms?show=all";
                body_string(send(&router, "GET", path, Some(&token), None).await).await
            };

            assert!(list().await.contains("Highest 12.0"));
            assert!(state.memos.alarm_peaks.get(&ended).is_some(), "memoised");

            let mut later = portal_storage::MinuteReading {
                hostname: "h1".into(),
                sensor_line: 0,
                minute_ts: 1_787_011_320,
                min_c: Some(20.0),
                avg_c: Some(20.0),
                max_c: Some(20.0),
                sample_count: Some(6),
                disconnect_count: Some(0),
                worst_alarm: None,
                source: "live".into(),
                data_hmac: None,
            };
            storage.upsert_minute(&later).expect("minute");
            let html = list().await;
            assert!(
                html.contains("Highest 12.0") && !html.contains("Highest 20.0"),
                "{html}"
            );

            let open = seed_wired(dir.path(), "HIGH", Some(9.0), None);
            later.minute_ts += 60;
            later.avg_c = Some(30.0);
            storage.upsert_minute(&later).expect("minute");
            assert!(
                list().await.contains("Highest 30.0"),
                "the open alarm reads live"
            );
            assert!(state.memos.alarm_peaks.get(&open).is_none());
        }

        /// The peak is a value alarm's fact: a DISCONNECT has none, even on a
        /// line whose readings fill its window. A wired peak read off the
        /// stored minutes is a minute average and says so; the row's own
        /// sampled value is a plain "Highest value".
        #[tokio::test]
        async fn the_peak_shows_for_value_alarms_and_names_a_minute_average() {
            let (state, dir) = test_state().await;
            let disconnect = seed_wired(dir.path(), "DISCONNECT", None, Some(12.0));
            let averaged = seed_wired(dir.path(), "HIGH", Some(9.0), Some(12.0));
            let sampled = seed_wired(dir.path(), "HIGH", Some(15.0), Some(12.0));
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state);
            let page = |id: String| {
                let router = router.clone();
                let token = token.clone();
                async move {
                    let path = format!("/alarms/{id}");
                    body_string(send(&router, "GET", &path, Some(&token), None).await).await
                }
            };

            let html = page(disconnect).await;
            assert!(!html.contains("Highest"), "{html}");
            let html = page(averaged).await;
            assert!(html.contains("Highest minute average"), "{html}");
            let html = page(sampled).await;
            assert!(html.contains("Highest value"), "{html}");
            assert!(!html.contains("minute average"), "{html}");
        }

        /// LE-17: a viewer is not offered Acknowledge or the note form, a manager is.
        #[tokio::test]
        async fn a_viewer_is_not_offered_the_alarm_actions() {
            let (state, dir) = test_state().await;
            let id = seed_alarm(dir.path());
            let viewer = crate::routes::test_support::login_as_role(
                &state,
                crate::auth::Role::Viewer,
                "viewer1",
            )
            .await;
            let manager = crate::routes::test_support::login_as_role(
                &state,
                crate::auth::Role::Manager,
                "manager1",
            )
            .await;
            let router = super::super::router().with_state(state.clone());
            let page = |token: String| {
                let router = router.clone();
                let id = id.clone();
                async move {
                    body_string(
                        send(&router, "GET", &format!("/alarms/{id}"), Some(&token), None).await,
                    )
                    .await
                }
            };
            let viewer_list = viewer.clone();
            let as_viewer = page(viewer).await;
            assert!(!as_viewer.contains("/notes\""), "no note form for a viewer");
            assert!(
                !as_viewer.contains("/acknowledge\""),
                "no acknowledge for a viewer"
            );
            let as_manager = page(manager.clone()).await;
            assert!(as_manager.contains("/notes\""), "{as_manager}");

            // Nor the list's bulk acknowledge (the table half of LE-17).
            let list = |token: String| {
                let router = router.clone();
                async move {
                    body_string(send(&router, "GET", "/alarms", Some(&token), None).await).await
                }
            };
            let as_viewer = list(viewer_list).await;
            assert!(
                !as_viewer.contains("id=\"alarm-bulk-bar\""),
                "no bulk bar for a viewer"
            );
            assert!(!as_viewer.contains("class=\"alarm-row-checkbox"));
            let as_manager = list(manager).await;
            assert!(as_manager.contains("id=\"alarm-bulk-bar\""));
            assert!(
                as_manager.contains("class=\"alarm-row-checkbox"),
                "{as_manager}"
            );
        }

        #[tokio::test]
        async fn unknown_alarm_is_404_for_detail_and_posts() {
            let (state, _dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state);

            let response = send(&router, "GET", "/alarms/no-such-id", Some(&token), None).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            // A pruned alarm reached from an e-mail says so in the reader's
            // words (UX-026), inside the layout with the navigation, like
            // every other missing page.
            let html = body_string(response).await;
            assert!(html.contains("This alarm no longer exists."), "{html}");
            assert!(html.contains("<nav") && html.contains(">404<"), "{html}");

            for path in ["/alarms/no-such-id/acknowledge", "/alarms/no-such-id/notes"] {
                let response = send(
                    &router,
                    "POST",
                    path,
                    Some(&token),
                    Some("reason=whatever&content=whatever"),
                )
                .await;
                assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
                let html = body_string(response).await;
                assert!(
                    html.contains("This alarm no longer exists."),
                    "{path}: {html}"
                );
                assert!(html.contains(">404<"), "{path}: {html}");
            }
        }

        #[tokio::test]
        async fn bulk_ack_acknowledges_each_id_with_its_own_audit_row() {
            use portal_storage::AuditFilter;

            let (state, dir) = test_state().await;
            let id1 = seed_alarm(dir.path());
            let id2 = seed_alarm(dir.path());
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state.clone());

            let body = format!("ids[]={id1}&ids[]={id2}&reason=confirmed+on-site");
            let response = send_htmx(&router, "/alarms/acknowledge", &token, &body).await;
            assert_eq!(response.status(), StatusCode::OK);
            let html = body_string(response).await;
            assert!(html.contains("id=\"alarm-table\""), "table re-rendered");
            assert!(
                html.contains("Acknowledged 2 alarms, 0 skipped"),
                "summary line present:\n{html}"
            );
            assert!(
                html.contains(r#"id="page-stats" hx-swap-oob="true""#),
                "the metric row follows the list:\n{html}"
            );

            let (entries, total) = state
                .auth
                .audit_page(AuditFilter {
                    event_type: Some("alarm_ack".to_string()),
                    limit: 100,
                    ..Default::default()
                })
                .await
                .expect("audit page");
            assert_eq!(total, 2, "one audit row per acknowledged alarm");
            let targets: Vec<_> = entries.iter().filter_map(|e| e.target.clone()).collect();
            assert!(targets.contains(&id1) && targets.contains(&id2));
        }

        #[tokio::test]
        async fn bulk_ack_with_blank_reason_renders_error_not_500() {
            let (state, dir) = test_state().await;
            let id = seed_alarm(dir.path());
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state);

            let body = format!("ids[]={id}&reason=+++");
            let response = send_htmx(&router, "/alarms/acknowledge", &token, &body).await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "error re-render, not a 4xx/500"
            );
            let html = body_string(response).await;
            assert!(
                html.contains("alert-error"),
                "error banner present:\n{html}"
            );
            assert!(html.contains("id=\"alarm-table\""), "table swap target");
        }

        /// decisions.md #28d: an alarm that ended before anyone acknowledged
        /// it can still be acknowledged, so the trail can be completed. The
        /// detail offers the form, the list the checkbox, and the bulk route
        /// takes it; the reason stays required.
        #[tokio::test]
        async fn an_ended_alarm_can_be_acknowledged_from_the_detail_and_the_list() {
            let (state, dir) = test_state().await;
            let id = seed_alarm(dir.path());
            let id2 = seed_alarm(dir.path());
            let storage = portal_storage::Storage::open(&dir.path().join("portal.db"), None)
                .expect("open storage");
            for id in [&id, &id2] {
                storage
                    .clear_alarm(id, "2026-08-18T00:10:00Z", None)
                    .expect("clear");
            }
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state);

            let detail = body_string(
                send(&router, "GET", &format!("/alarms/{id}"), Some(&token), None).await,
            )
            .await;
            assert!(
                detail.contains(&format!("hx-post=\"/alarms/{id}/acknowledge\"")),
                "the detail offers the acknowledgement:\n{detail}"
            );

            let list =
                body_string(send(&router, "GET", "/alarms?show=all", Some(&token), None).await)
                    .await;
            assert!(
                list.contains(&format!("name=\"ids[]\" value=\"{id}\"")),
                "the list offers the checkbox:\n{list}"
            );

            let blank = format!("ids[]={id}&reason=+++");
            let html =
                body_string(send_htmx(&router, "/alarms/acknowledge", &token, &blank).await).await;
            assert!(html.contains("alert-error"), "a reason is still required");

            let body = format!("ids[]={id}&reason=reviewed+the+log");
            let html =
                body_string(send_htmx(&router, "/alarms/acknowledge", &token, &body).await).await;
            assert!(
                html.contains("Acknowledged 1 alarms, 0 skipped"),
                "the bulk route takes an ended alarm:\n{html}"
            );
            let single = send(
                &router,
                "POST",
                &format!("/alarms/{id2}/acknowledge"),
                Some(&token),
                Some("reason=reviewed"),
            )
            .await;
            assert_eq!(single.status(), StatusCode::OK);
            for id in [&id, &id2] {
                let row = storage.get_alarm(id).unwrap().unwrap();
                assert!(row.acknowledged_at.is_some(), "acknowledged: {id}");
            }
        }

        /// decisions.md #30d (UX-018): an alarm of a Hub the registry does
        /// not know reads "state unknown" in the list and the detail, is not
        /// counted by the badge or the metric row, and links nowhere a 404
        /// waits. The row stays listed: it is a real record.
        #[tokio::test]
        async fn an_alarm_of_an_unknown_hub_reads_state_unknown_and_is_not_counted() {
            let (state, dir) = test_state().await;
            let id = seed_alarm(dir.path());
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state.clone());

            let badge =
                body_string(send(&router, "GET", "/alarms/badge", Some(&token), None).await).await;
            assert!(!badge.contains("bg-error"), "not counted:\n{badge}");
            assert_eq!(state.alarms.cached_active_count(), 0);

            let list = body_string(send(&router, "GET", "/alarms", Some(&token), None).await).await;
            assert!(list.contains("Fridge"), "still listed");
            assert!(list.contains("State unknown"), "marked:\n{list}");

            let detail = body_string(
                send(&router, "GET", &format!("/alarms/{id}"), Some(&token), None).await,
            )
            .await;
            assert!(detail.contains("State unknown"), "marked:\n{detail}");
            assert!(!detail.contains("Open device"), "no link to a 404");

            // The Hub comes back into the registry: counted and linked again.
            state.store.apply(crate::state::IngestEvent::Status {
                hostname: "h1".into(),
                status: fiber_protocol::payloads::status::DeviceStatus::Online,
            });
            let badge =
                body_string(send(&router, "GET", "/alarms/badge", Some(&token), None).await).await;
            assert!(badge.contains(">1<"), "counted:\n{badge}");
            let detail = body_string(
                send(&router, "GET", &format!("/alarms/{id}"), Some(&token), None).await,
            )
            .await;
            assert!(!detail.contains("State unknown"));
            assert!(detail.contains("Open device"), "linked again");
        }

        #[tokio::test]
        async fn bulk_ack_skips_an_already_acknowledged_alarm() {
            let (state, dir) = test_state().await;
            let id = seed_alarm(dir.path());
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state);

            // First ack goes through the single-alarm route.
            let first = send(
                &router,
                "POST",
                &format!("/alarms/{id}/acknowledge"),
                Some(&token),
                Some("reason=first+pass"),
            )
            .await;
            assert_eq!(first.status(), StatusCode::OK);

            // The bulk route is asked to acknowledge the same id again.
            let body = format!("ids[]={id}&reason=second+pass");
            let response = send_htmx(&router, "/alarms/acknowledge", &token, &body).await;
            assert_eq!(response.status(), StatusCode::OK);
            let html = body_string(response).await;
            assert!(
                html.contains("Acknowledged 0 alarms, 1 skipped"),
                "already-acked id counted as skipped, not re-acked:\n{html}"
            );
        }

        #[tokio::test]
        async fn list_page_renders_seeded_alarm() {
            let (state, dir) = test_state().await;
            seed_alarm(dir.path());
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state);

            let response = send(&router, "GET", "/alarms", Some(&token), None).await;
            assert_eq!(response.status(), StatusCode::OK);
            let html = body_string(response).await;
            assert!(
                html.contains("Fridge"),
                "seeded alarm row rendered:\n{html}"
            );
            assert!(html.contains("id=\"alarm-table\""));
        }

        /// UX-040: a Beacon value alarm names the Beacon ("Freezer") and the
        /// unit in the list row, the detail header and the toast, although
        /// its `sensor_id` carries a MAC with colons of its own.
        #[tokio::test]
        async fn a_beacon_alarm_names_the_beacon_and_the_unit_everywhere() {
            let (state, dir) = test_state().await;
            state.store.apply(crate::state::IngestEvent::EyeSensors {
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
            let storage =
                portal_storage::Storage::open(&dir.path().join("portal.db"), None).expect("open");
            let id = storage
                .insert_alarm_event(&portal_storage::NewAlarmEvent {
                    hostname: "h1".into(),
                    sensor_id: "eye:EE:E0:00:00:00:01:Temperature".into(),
                    sensor_name: Some("Temperature".into()),
                    sensor_location: None,
                    alarm_type: "HIGH".into(),
                    severity: "critical".into(),
                    from_state: None,
                    to_state: Some("CRITICAL".into()),
                    message: None,
                    value: Some(4.44),
                    threshold: Some(-10.0),
                    triggered_at: "2026-09-26T05:37:51Z".into(),
                    cleared_at: None,
                    acknowledged_at: None,
                    acknowledged_by_user_id: None,
                    acknowledged_by_username: None,
                    native_key: None,
                })
                .expect("seed");
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state.clone());

            for path in ["/alarms".to_string(), format!("/alarms/{id}")] {
                let html = body_string(send(&router, "GET", &path, Some(&token), None).await).await;
                assert!(html.contains("Freezer"), "{path}: {html}");
                let html = html.replace("&nbsp;", " ");
                assert!(html.contains("4.4 °C"), "{path}: {html}");
                assert!(!html.contains("eye:EE:E0"), "{path}: raw id shown: {html}");
            }

            let alarm = storage.get_alarm(&id).expect("read").expect("row");
            let ctx = crate::render::alarm_ctx(&alarm, &state.store.snapshot());
            let toast = body_string(crate::routes::render_fragment(
                &state,
                "partials/alarm_toast.html",
                minijinja::context! { a => ctx },
            ))
            .await
            .replace("&nbsp;", " ");
            assert!(
                toast.contains("Freezer") && toast.contains("4.4 °C"),
                "{toast}"
            );
        }
    }
}
