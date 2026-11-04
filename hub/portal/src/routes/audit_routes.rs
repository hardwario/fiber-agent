//! `GET /audit` — manager and above (decision #18, 2026-09-26: the rows carry every client's
//! address and the names typed at failed logins, which a viewer has no business reading). Context
//! contract: `templates/pages/audit.html` + `templates/partials/audit_table.html`
//! header comments.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use minijinja::context;
use portal_storage::{AlarmListFilter, AlarmSort, AuditEntry, AuditFilter, AuditSort};
use serde::{Deserialize, Serialize};

use crate::auth::RequireManager;
use crate::http::AppState;
use crate::render;

use super::{non_empty, render as render_page};

const DEFAULT_LIMIT: i64 = 50;

pub fn router() -> Router<AppState> {
    Router::new().route("/audit", get(audit_page))
}

#[derive(Deserialize)]
struct AuditQuery {
    event_type: Option<String>,
    username: Option<String>,
    /// `"1"` / `"0"`, the only two the control offers. Anything else is
    /// treated as no filter rather than as an error: a hand-edited URL should
    /// widen the view, not 400.
    success: Option<String>,
    /// `YYYY-MM-DD` from a date input, compared as a string against `ts`
    /// (`datetime('now')`, i.e. `YYYY-MM-DD HH:MM:SS`) — which is why the
    /// upper bound has to be widened to the end of its day, below.
    since: Option<String>,
    until: Option<String>,
    /// Free-text search. A SQL clause, not a pass over the rendered page:
    /// the log paginates, so a client-side search answered "nothing" for
    /// everything outside the fifty rows on screen.
    q: Option<String>,
    /// `24h`: only the last 24 hours, the window the metric tiles count
    /// (`AuditCounts`). A rolling window, not a day of the display zone, so
    /// it is its own param rather than a value of `since`. An explicit
    /// `since` wins; anything other than `24h` is no filter.
    window: Option<String>,
    /// `alarm` | `warning` | `log`: one kind of event (the tiles link these).
    /// Anything else is all three.
    #[serde(rename = "type")]
    kind: Option<String>,
    /// Sort column key (`AuditSort::key`); an unknown one falls back to the
    /// log's own order rather than erroring.
    sort: Option<String>,
    /// `asc` | `desc`; anything else means the column's default.
    dir: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// `partials/audit_table.html`'s row shape (`user_id` dropped, the
/// template never reads it; the actor cell shows `user_label`).
#[derive(Serialize)]
struct AuditEntryCtx {
    id: i64,
    ts: String,
    event_type: String,
    username: Option<String>,
    /// `username` as a reader sees it, `"Jan Novák (admin)"` or the bare
    /// username (`render::person_label`).
    user_label: Option<String>,
    ip: Option<String>,
    target: Option<String>,
    /// Where the target cell links, see [`target_href`]; `None` renders it
    /// as text.
    target_href: Option<String>,
    details: Option<String>,
    /// `details` re-indented when it is JSON, for the record dialog; `None`
    /// when it is prose (then the dialog shows `details` as is).
    details_pretty: Option<String>,
    success: bool,
    /// The row was written by the maintenance schedule rather than a person, so
    /// the actor cell says "scheduled" instead of the "–" it shows for a
    /// genuinely absent value. Decided here rather than in the template: which
    /// events are actor-less by design is state, not presentation, and a
    /// template matching on event-type prefixes would drift from Rust silently.
    scheduled: bool,
}

impl AuditEntryCtx {
    fn new(
        e: &AuditEntry,
        full_names: &HashMap<String, String>,
        snapshot: &crate::state::PortalState,
    ) -> Self {
        Self {
            target_href: target_href(e, snapshot),
            id: e.id,
            ts: e.ts.clone(),
            event_type: e.event_type.clone(),
            username: e.username.clone(),
            user_label: e
                .username
                .as_deref()
                .map(|u| render::person_label(u, full_names)),
            ip: e.ip.clone(),
            target: e.target.clone(),
            details: e.details.clone(),
            details_pretty: e.details.as_deref().and_then(pretty_json),
            success: e.success,
            scheduled: e.username.is_none() && e.event_type.starts_with("database_maintenance"),
        }
    }
}

/// `details` pretty-printed when it parses as a JSON object or array.
fn pretty_json(raw: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    if !(v.is_object() || v.is_array()) {
        return None;
    }
    serde_json::to_string_pretty(&v).ok()
}

/// The UTC stamp 24 hours ago, in the `ts` column's shape.
fn last_day_bound() -> String {
    jiff::Timestamp::now()
        .checked_sub(jiff::SignedDuration::from_hours(24))
        .unwrap_or_else(|_| jiff::Timestamp::now())
        .strftime("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// The page a target names: one alarm, or a Node or Beacon the portal still
/// knows. A removed device's page is a 404, so its target stays plain text
/// (UX-052); group ids, hostnames and free text never link.
fn target_href(e: &AuditEntry, snapshot: &crate::state::PortalState) -> Option<String> {
    let t = e
        .target
        .as_deref()
        .filter(|t| !t.is_empty() && !t.contains(' '))?;
    if e.event_type == "alarm_ack" {
        Some(format!("/alarms/{t}"))
    } else if e.event_type.starts_with("node_") {
        snapshot
            .hub_of_node(t)
            .map(|_| format!("/nodes/{t}/overview"))
    } else if e.event_type.starts_with("beacon_") {
        snapshot
            .hub_of_beacon(t)
            .map(|_| format!("/beacons/{t}/overview"))
    } else {
        None
    }
}

/// The operator picks a DAY of the display zone (decision #25); `ts` is a
/// UTC `datetime('now')` stamp. So each bound becomes that day's first or
/// last second in the zone, written in UTC (UX-042): a bare
/// `ts <= '2026-09-14'` would cut the day off at midnight UTC, and a Prague
/// day starts two hours before a UTC one in summer.
///
/// A value that is not a plain `YYYY-MM-DD` is passed through untouched
/// rather than rejected: the control only ever emits that shape, and a
/// hand-edited URL should not be able to turn a filter into an error page.
fn since_bound(day: Option<&str>) -> Option<String> {
    day.map(|d| crate::display_tz::day_bound_utc(d, false).unwrap_or_else(|| d.to_string()))
}

fn until_bound(day: Option<&str>) -> Option<String> {
    day.map(|d| crate::display_tz::day_bound_utc(d, true).unwrap_or_else(|| d.to_string()))
}

async fn audit_page(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Query(q): Query<AuditQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 1000);
    let offset = q.offset.unwrap_or(0).max(0);
    let event_type = q.event_type.and_then(non_empty);
    let username = q.username.and_then(non_empty);
    let q_search = q.q.and_then(non_empty);
    let success = q.success.as_deref().and_then(|v| match v {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    });
    let since = q.since.clone().and_then(non_empty);
    let until = q.until.clone().and_then(non_empty);
    let window = q.window.clone().filter(|w| w == "24h");

    let sort = q
        .sort
        .as_deref()
        .and_then(AuditSort::from_key)
        .unwrap_or_default();
    // Time reads newest-first, the rest A-Z, unless `dir` says otherwise.
    let sort_desc = match q.dir.as_deref() {
        Some("asc") => false,
        Some("desc") => true,
        _ => sort == AuditSort::Time,
    };
    // What a sort header carries over: the filters, not `offset` — a
    // re-sorted log starts at page one.
    // Which empty state the table shows; see the same flag on /alarms.
    let filtered = event_type.is_some()
        || username.is_some()
        || q_search.is_some()
        || success.is_some()
        || since.is_some()
        || until.is_some()
        || window.is_some();
    let sort_qs = super::filter_qs(&[
        ("event_type", event_type.clone()),
        ("username", username.clone()),
        ("q", q_search.clone()),
        ("success", q.success.clone().and_then(non_empty)),
        ("since", since.clone()),
        ("until", until.clone()),
        ("window", window.clone()),
    ]);

    let kind = q
        .kind
        .clone()
        .filter(|k| matches!(k.as_str(), "alarm" | "warning" | "log"));
    let filtered = filtered || kind.is_some();
    let sort_qs = match kind.as_deref() {
        Some(k) if sort_qs.is_empty() => format!("type={k}"),
        Some(k) => format!("{sort_qs}&type={k}"),
        None => sort_qs,
    };
    let since_utc =
        since_bound(since.as_deref()).or_else(|| window.as_ref().map(|_| last_day_bound()));
    let until_utc = until_bound(until.as_deref());

    // Which logs the view reads. The actor, event and result filters are
    // the operator log's own columns, so any of them leaves the alarms out.
    let log_only = event_type.is_some() || username.is_some() || success.is_some();
    let want_log = matches!(kind.as_deref(), None | Some("log"));
    let want_alarm = !log_only && matches!(kind.as_deref(), None | Some("alarm"));
    let want_warning = !log_only && matches!(kind.as_deref(), None | Some("warning"));
    let mixed = [want_log, want_alarm, want_warning]
        .iter()
        .filter(|w| **w)
        .count()
        > 1;
    // A merged view pages over both sources by time: each source is read
    // from its start up to the end of the page, the two merged newest-first
    // and the page cut out. One source pages in SQL as before.
    let (src_limit, src_offset) = if mixed {
        ((offset + limit).min(1000), 0)
    } else {
        (limit, offset)
    };

    let (event_types, usernames) = state
        .auth
        .audit_filter_options()
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, "failed to read audit filter options");
            (Vec::new(), Vec::new())
        });

    // The metric row: whole-log counts, unaffected by the filters below it
    // — see `render::audit_stats_ctx`.
    let severity_total = |sev: &'static str| {
        let alarms = state.alarms.clone();
        async move {
            alarms
                .list(AlarmListFilter {
                    limit: 1,
                    severity: Some(sev.into()),
                    ..Default::default()
                })
                .await
                .map(|(_, n)| n)
                .unwrap_or(0)
        }
    };
    let stats = render::audit_stats_ctx(
        state.store.snapshot_mqtt().connected,
        state.auth.audit_counts().await.unwrap_or_else(|err| {
            tracing::warn!(%err, "failed to read audit counts");
            Default::default()
        }),
        severity_total("critical").await,
        severity_total("warning").await,
    );

    let mut total = 0i64;
    let mut log_entries = Vec::new();
    if want_log {
        let filter = AuditFilter {
            event_type: event_type.clone(),
            username: username.clone(),
            since: since_utc.clone(),
            until: until_utc.clone(),
            success,
            search: q_search.clone(),
            limit: src_limit,
            offset: src_offset,
            // A merged view orders by time; the other columns sort one log.
            sort: if mixed { AuditSort::Time } else { sort },
            sort_desc: if mixed { true } else { sort_desc },
        };
        match state.auth.audit_page(filter).await {
            Ok((entries, n)) => {
                total += n;
                log_entries = entries;
            }
            Err(err) => {
                tracing::error!(%err, "failed to load audit page");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to load audit log",
                )
                    .into_response();
            }
        }
    }
    let mut alarm_events = Vec::new();
    for (wanted, sev) in [(want_alarm, "critical"), (want_warning, "warning")] {
        if !wanted {
            continue;
        }
        let filter = AlarmListFilter {
            limit: src_limit,
            offset: src_offset,
            severity: Some(sev.into()),
            search: q_search.clone(),
            triggered_since: since_utc.clone(),
            triggered_until: until_utc.clone(),
            sort: AlarmSort::Start,
            sort_desc: mixed || sort != AuditSort::Time || sort_desc,
            ..Default::default()
        };
        match state.alarms.list(filter).await {
            Ok((rows, n)) => {
                total += n;
                alarm_events.extend(rows);
            }
            Err(err) => tracing::warn!(%err, "failed to read alarms for the audit log"),
        }
    }

    // One lookup for the page's actors, not one per row.
    let names = state
        .db
        .full_names(
            log_entries
                .iter()
                .filter_map(|e| e.username.clone())
                .collect(),
        )
        .await;
    let snapshot = state.store.snapshot();
    let mut rows: Vec<EventRowCtx> = log_entries
        .iter()
        .map(|e| EventRowCtx::log(AuditEntryCtx::new(e, &names, &snapshot)))
        .chain(
            alarm_events
                .iter()
                .map(|a| EventRowCtx::alarm(render::alarm_ctx(a, &snapshot))),
        )
        .collect();
    if mixed {
        rows.sort_by(|a, b| b.sort_ts.cmp(&a.sort_ts));
        rows = rows
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
    }

    let ctx = render::page_ctx(
        context! {
            nav_active => "audit",
            stats => stats,
            entries => rows,
            total => total,
            filter => context! {
                event_type => event_type,
                username => username,
                q => q_search,
                // Back as the strings the controls emit, not as the
                // widened bound the query used.
                success => q.success.clone().and_then(non_empty),
                since => since,
                until => until,
                window => window,
                kind => kind,
            },
            event_types => event_types,
            usernames => usernames,
            limit => limit,
            offset => offset,
            filtered => filtered,
            sort => sort.key(),
            dir => if sort_desc { "desc" } else { "asc" },
            sort_qs => sort_qs,
        },
        Some(&current_user),
    );
    render_page(&state, "pages/audit.html", ctx)
}

/// One row of the merged event log: an operator-log entry or an alarm
/// (`kind` `log` | `alarm` | `warning`), with the UTC instant the merge
/// orders by. The template branches on `kind`.
#[derive(Serialize)]
struct EventRowCtx {
    kind: &'static str,
    /// `YYYY-MM-DD HH:MM:SS` UTC for both sources (an alarm's RFC 3339
    /// `triggered_at` with its `T` read as a space).
    sort_ts: String,
    /// Unique across both sources, for the row's DOM id.
    key: String,
    log: Option<AuditEntryCtx>,
    alarm: Option<render::AlarmCtx>,
}

impl EventRowCtx {
    fn log(e: AuditEntryCtx) -> Self {
        Self {
            kind: "log",
            sort_ts: e.ts.clone(),
            key: format!("log-{}", e.id),
            log: Some(e),
            alarm: None,
        }
    }

    fn alarm(a: render::AlarmCtx) -> Self {
        Self {
            kind: if a.max_severity.eq_ignore_ascii_case("warning") {
                "warning"
            } else {
                "alarm"
            },
            sort_ts: utc_sort_key(&a.triggered_at_raw),
            key: format!("alarm-{}", a.id),
            log: None,
            alarm: Some(a),
        }
    }
}

/// An RFC 3339 UTC stamp in `audit_log.ts`'s shape.
fn utc_sort_key(rfc3339: &str) -> String {
    rfc3339.get(..19).unwrap_or(rfc3339).replace('T', " ")
}

/// Smoke coverage for `pages/audit.html`/`partials/audit_table.html` — this
/// milestone translated their labels and pagination summary; `audit_routes.rs`
/// had no tests before M9.
#[cfg(test)]
mod tests {
    use super::until_bound;

    /// The day bound, and the off-by-most-of-a-day it exists to prevent.
    #[test]
    fn the_period_upper_bound_covers_the_whole_day() {
        assert_eq!(
            until_bound(Some("2026-09-14")).as_deref(),
            Some("2026-09-14 23:59:59"),
            "a bare date as an upper bound would cut the day off at midnight"
        );
        assert_eq!(until_bound(None), None, "no bound stays no bound");
        // Not a date the control could have produced: passed through, not
        // rejected — a hand-edited URL should widen or narrow the view, never
        // become an error page.
        assert_eq!(until_bound(Some("yesterday")).as_deref(), Some("yesterday"));
        assert_eq!(
            until_bound(Some("2026-09-14 12:00:00")).as_deref(),
            Some("2026-09-14 12:00:00"),
            "an explicit time is already precise and is left alone"
        );
    }

    use axum::body::to_bytes;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    use crate::auth::SESSION_COOKIE;
    use crate::routes::test_support::{login, test_state};

    /// UX-052: a removed Node's audit row names it as text, since its page
    /// is a 404; a Node that still exists and an alarm keep their links.
    #[test]
    fn a_removed_device_target_is_text_not_a_dead_link() {
        let entry = |event_type: &str, target: &str| portal_storage::AuditEntry {
            id: 1,
            ts: "2026-09-26 06:00:00".into(),
            event_type: event_type.into(),
            username: Some("admin".into()),
            user_id: Some(1),
            ip: None,
            target: Some(target.into()),
            details: None,
            success: true,
        };
        let mut snapshot = crate::state::PortalState::default();
        let mut hub = crate::state::DeviceState::new("h1".into());
        let node = crate::state::NodeState::new("70b3d50000000001".into(), 0);
        hub.nodes.insert(node.node_key.clone(), node);
        snapshot.devices.insert("h1".into(), hub);

        assert_eq!(
            super::target_href(&entry("node_remove", "70b3d500000000ff"), &snapshot),
            None
        );
        assert_eq!(
            super::target_href(&entry("node_rename", "70b3d50000000001"), &snapshot).as_deref(),
            Some("/nodes/70b3d50000000001/overview")
        );
        assert_eq!(
            super::target_href(&entry("alarm_ack", "a1"), &snapshot).as_deref(),
            Some("/alarms/a1")
        );
    }

    #[tokio::test]
    async fn audit_page_renders_the_setup_entry() {
        let (state, _dir) = test_state().await;
        let token = login(&state, "admin", "correct-horse-battery").await;
        let router = super::router().with_state(state);

        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/audit")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        // `test_state()`'s `create_superuser` writes one "setup" audit
        // entry, so the table renders a real row rather than the empty
        // state — this exercises the translated table headers.
        assert!(body.contains("setup"));
        assert!(!body.contains("auth.audit"));
        // No full name on file: the actor cell is the bare username.
        assert!(body.contains(r#"<span class="badge-outline-v1">admin</span>"#));
    }

    /// decisions.md #28e: a refused privileged action writes one failed audit
    /// row naming the user and the route, and the Audit page labels it.
    #[tokio::test]
    async fn a_refused_privileged_action_is_audited_once() {
        let (state, _dir) = test_state().await;
        let viewer = crate::routes::test_support::login_as_role(
            &state,
            crate::auth::Role::Viewer,
            "viewer1",
        )
        .await;
        let router = super::router()
            .merge(crate::routes::notification_routes::router())
            .with_state(state.clone());
        let request = |method: &str, uri: &str, token: &str| {
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                .body(Body::empty())
                .unwrap()
        };

        let refused = router
            .clone()
            .oneshot(request("POST", "/settings/notifications", &viewer))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);

        let (rows, _) = state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some("permission_denied".into()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(!rows[0].success);
        assert_eq!(rows[0].username.as_deref(), Some("viewer1"));
        assert_eq!(
            rows[0].target.as_deref(),
            Some("POST /settings/notifications")
        );

        let admin = login(&state, "admin", "correct-horse-battery").await;
        let page = router
            .oneshot(request("GET", "/audit?success=0", &admin))
            .await
            .unwrap();
        let bytes = to_bytes(page.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("Action refused"), "{body}");
        // minijinja escapes the slashes.
        assert!(body
            .replace("&#x2f;", "/")
            .contains("POST /settings/notifications"));
    }

    /// With a full name on file the actor cell names the person, the username
    /// still beside it (the value the page's user filter takes).
    #[tokio::test]
    async fn audit_actor_shows_the_full_name_beside_the_username() {
        let (state, _dir) = test_state().await;
        state
            .db
            .run(|s| {
                let admin = s.get_user_by_username("admin")?.expect("seeded admin");
                s.update_user(admin.id, Some(Some("Jan Novák")), None)
            })
            .await
            .unwrap();
        let token = login(&state, "admin", "correct-horse-battery").await;
        let router = super::router().with_state(state);

        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/audit")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains(r#"<span class="badge-outline-v1">Jan Novák (admin)</span>"#),
            "{body}"
        );
    }

    /// JSON details are re-indented for the record dialog; prose is not.
    #[test]
    fn json_details_are_pretty_printed_for_the_dialog() {
        assert_eq!(
            super::pretty_json(r#"{"from":"a","to":"b"}"#).as_deref(),
            Some("{\n  \"from\": \"a\",\n  \"to\": \"b\"\n}")
        );
        assert_eq!(super::pretty_json("renamed by hand"), None);
        assert_eq!(
            super::pretty_json("42"),
            None,
            "a bare scalar is not a record"
        );
    }

    fn seed_alarm(dir: &std::path::Path, severity: &str, triggered_at: &str) {
        let storage =
            portal_storage::Storage::open(&dir.join("portal.db"), None).expect("open storage");
        storage
            .insert_alarm_event(&portal_storage::NewAlarmEvent {
                hostname: "h1".into(),
                // One open alarm per sensor, so each severity gets its own.
                sensor_id: if severity == "warning" {
                    "line-1"
                } else {
                    "line-0"
                }
                .into(),
                sensor_name: Some(format!("Fridge {severity}")),
                sensor_location: None,
                alarm_type: "HIGH".into(),
                severity: severity.into(),
                from_state: Some("NORMAL".into()),
                to_state: Some(severity.to_uppercase()),
                message: None,
                value: Some(9.0),
                threshold: Some(8.0),
                triggered_at: triggered_at.into(),
                cleared_at: None,
                acknowledged_at: None,
                acknowledged_by_user_id: None,
                acknowledged_by_username: None,
                native_key: None,
            })
            .expect("seed alarm");
    }

    /// One event log: alarms and warnings beside the operator log, the
    /// tiles count each and link `?type=`, every row carries its record for
    /// the dialog, and the 24-hour window bounds the alarms too.
    #[tokio::test]
    async fn audit_merges_alarms_warnings_and_the_log() {
        let (state, dir) = test_state().await;
        seed_alarm(dir.path(), "critical", "2026-08-18T00:00:00Z");
        seed_alarm(dir.path(), "warning", "2026-08-19T00:00:00Z");
        let token = login(&state, "admin", "correct-horse-battery").await;
        let router = super::router().with_state(state);
        let get = |uri: &'static str| {
            let router = router.clone();
            let token = token.clone();
            async move {
                let r = router
                    .oneshot(
                        Request::builder()
                            .uri(uri)
                            .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(r.status(), StatusCode::OK);
                let b = to_bytes(r.into_body(), usize::MAX).await.unwrap();
                String::from_utf8(b.to_vec()).unwrap()
            }
        };
        let all = get("/audit").await;
        assert!(all.contains(r#"data-kind="alarm""#), "{all}");
        assert!(all.contains(r#"data-kind="warning""#));
        assert!(all.contains(r#"data-kind="log""#));
        assert!(all.contains(r#"id="audit-record""#));
        assert!(all.contains("data-audit-record"));
        assert!(all.contains("audit?type=warning"));
        // Newest first across both sources: the setup log row (now) leads.
        let log_at = all.find(r#"data-kind="log""#).unwrap();
        let warn_at = all.find(r#"data-kind="warning""#).unwrap();
        let alarm_at = all.find(r#"data-kind="alarm""#).unwrap();
        assert!(log_at < warn_at && warn_at < alarm_at);

        let alarms = get("/audit?type=alarm").await;
        assert!(alarms.contains(r#"data-kind="alarm""#));
        assert!(!alarms.contains(r#"data-kind="warning""#));
        assert!(!alarms.contains(r#"data-kind="log""#));

        let logs = get("/audit?type=log").await;
        assert!(logs.contains(r#"data-kind="log""#));
        assert!(!logs.contains(r#"data-kind="alarm""#));

        // A log-only filter leaves the alarms out.
        let failed = get("/audit?success=1").await;
        assert!(!failed.contains(r#"data-kind="alarm""#));

        let recent = get("/audit?window=24h").await;
        assert!(recent.contains(r#"title="setup""#));
        assert!(!recent.contains(r#"data-kind="alarm""#), "{recent}");
        assert!(recent.contains(r#"<option value="24h" selected>"#));
        // The bound is a UTC stamp in the `ts` column's shape.
        assert_eq!(super::last_day_bound().len(), 19);
    }

    /// Decision #18: a viewer is refused, a manager reads it.
    #[tokio::test]
    async fn the_audit_log_is_for_manager_and_above() {
        let (state, _dir) = test_state().await;
        let router = super::router().with_state(state.clone());
        let status = |token: String| {
            let router = router.clone();
            async move {
                router
                    .oneshot(
                        Request::builder()
                            .uri("/audit")
                            .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
                    .status()
            }
        };
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
        assert_eq!(status(viewer).await, StatusCode::FORBIDDEN);
        assert_eq!(status(manager).await, StatusCode::OK);
    }
}
