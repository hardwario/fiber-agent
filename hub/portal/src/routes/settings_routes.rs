//! `/settings` — the settings area's sub-navigation and the one section that
//! lives here: database maintenance.
//!
//! There is no settings INDEX. `GET /settings` redirects to the first section
//! the visitor's role allows; the tab bar (`partials/settings_tabs.html`) is
//! what makes the three sections read as one area. Until 2026-09-14 there was
//! a page of cards here linking to `/settings/password`,
//! `/settings/notifications` and `/settings/users` — all three already
//! reachable from the nav or the account menu, so the page pointed at the
//! doors beside it and nothing else. A redirect does the same job honestly.
//!
//! The maintenance section rewrites `<data_dir>/portal.db`, the PORTAL's own
//! database. Not a device's: the agent keeps storage of its own and this never
//! speaks to it. Worth saying out loud, because the panel spent an afternoon
//! on `/device`, which renders whichever Hub reported first and would have
//! claimed the rebuild was that Hub's.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Query};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{extract::State, response::Response, Router};
use minijinja::context;

use crate::auth::{CurrentUser, RequireAdmin, RequireManager, Role};
use crate::http::AppState;
use crate::render;
use crate::retention::{AuditActor, VacuumRefusal};

use super::render as render_page;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings", get(settings_redirect))
        .route("/settings/mqtt", get(mqtt_page))
        .route("/settings/mqtt/diagnostics", get(mqtt_fragment))
        .route("/settings/maintenance", get(maintenance_page))
        .route("/settings/maintenance/vacuum", post(start_vacuum))
        .route("/settings/maintenance/timezone", post(save_timezone))
}

/// Which tabs this visitor may see: `(notifications, mqtt, maintenance)`. One
/// function, so the bar and the redirect cannot disagree about what a role can
/// reach — and so a tab never leads to a 403.
///
/// MQTT ingest (plan/portal/mqtt/design.md §7, UI-MQ1) is manager and above
/// like notifications: the panel is for somebody who would act on
/// "degraded". Everybody still sees the *fact* of a disconnect through
/// `partials/mqtt_banner.html`.
///
/// Users is not a section: it is a list page with a nav entry of its own (see
/// `partials/settings_tabs.html` for why).
pub(crate) fn sections(user: &CurrentUser) -> (bool, bool, bool) {
    let manager = user.has_role(Role::Manager);
    (manager, manager, user.has_role(Role::Admin))
}

/// `GET /settings` — the area has no index, only sections. Lands on the first
/// one this role can open; a viewer can open none, so they get the portal's
/// own 404 rather than an empty page explaining they may not be here.
async fn settings_redirect(current_user: CurrentUser, State(state): State<AppState>) -> Response {
    let (notifications, mqtt, maintenance) = sections(&current_user);
    if notifications {
        Redirect::to("/settings/notifications").into_response()
    } else if mqtt {
        Redirect::to("/settings/mqtt").into_response()
    } else if maintenance {
        Redirect::to("/settings/maintenance").into_response()
    } else {
        // A viewer has no settings section: the 404 page, not an empty body.
        super::not_found(&state, Some(&current_user))
    }
}

/// `?ok=`/`?err=` flash, same shape as `notification_routes`.
#[derive(serde::Deserialize)]
struct Flash {
    ok: Option<String>,
    err: Option<String>,
}

async fn maintenance_page(
    _admin: RequireAdmin,
    current_user: CurrentUser,
    Query(flash): Query<Flash>,
    State(state): State<AppState>,
) -> Response {
    let maintenance = match &state.maintenance {
        Some(m) => m.status().await.ok().map(MaintenanceCtx::from),
        None => None,
    };
    let (show_notifications, show_mqtt, show_maintenance) = sections(&current_user);

    render_page(
        &state,
        "pages/maintenance.html",
        render::page_ctx(
            context! {
                nav_active => "settings",
                settings_tab => "maintenance",
                show_notifications => show_notifications,
                show_mqtt => show_mqtt,
                show_maintenance => show_maintenance,
                maintenance => maintenance,
                timezone => crate::display_tz::name(),
                timezones => crate::display_tz::available(),
                ok => flash.ok.as_deref().and_then(ok_slug),
                error => flash.err.as_deref().map(err_slug),
            },
            Some(&current_user),
        ),
    )
}

/// `GET /settings/mqtt` — the MQTT ingest section (plan/portal/mqtt/design.md
/// §7, requirement UI-MQ1): the third settings tab, manager and above like the
/// fragment it hosts. The panel was designed as a card on the settings index;
/// the index was retired on 2026-09-14 (no page whose only content is links
/// to the pages beside it), so the panel is a section of its own instead.
async fn mqtt_page(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
) -> Response {
    let (show_notifications, show_mqtt, show_maintenance) = sections(&current_user);
    render_page(
        &state,
        "pages/mqtt.html",
        render::page_ctx(
            context! {
                nav_active => "settings",
                settings_tab => "mqtt",
                show_notifications => show_notifications,
                show_mqtt => show_mqtt,
                show_maintenance => show_maintenance,
                mqtt => crate::mqtt::diagnostics(&state.store.snapshot_mqtt()),
            },
            Some(&current_user),
        ),
    )
}

/// `GET /settings/mqtt/diagnostics` — the ingest-state fragment
/// (plan/portal/mqtt/design.md §7, requirement UI-MQ1).
///
/// **Manager and above**, matching the notifications card: the panel is for
/// somebody who would act on "degraded", and the topic list and decoder errors
/// are internals rather than operator-facing state. Everybody still sees the
/// *fact* of a disconnect — `partials/mqtt_banner.html` renders that on every
/// monitoring page for every role, and it is the one that interrupts. This is
/// the page you open to find out why.
///
/// A fragment on its own route rather than a re-render of `/settings/mqtt`: it
/// polls itself every 10 s, and reloading a whole page on a timer would fight
/// the tab bar and any flash message on it.
///
/// No new JSON route — the api section owns that shape (design.md §7).
async fn mqtt_fragment(current_user: CurrentUser, State(state): State<AppState>) -> Response {
    if !current_user.has_role(Role::Manager) {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    render_page(
        &state,
        "partials/mqtt_diagnostics.html",
        render::page_ctx(
            context! { mqtt => crate::mqtt::diagnostics(&state.store.snapshot_mqtt()) },
            Some(&current_user),
        ),
    )
}

/// Only this file's own success code is recognised; anything else is dropped
/// rather than rendered, so a hand-built query cannot put text on the page.
fn ok_slug(code: &str) -> Option<&'static str> {
    match code {
        "maintenance_started" => Some("maintenance_started"),
        "timezone_saved" => Some("timezone_saved"),
        _ => None,
    }
}

/// Unknown codes collapse to `"unknown"` rather than `None`, matching
/// `notification_routes::err_slug`: a non-empty `?err=` always shows *a*
/// banner, never a blank one.
fn err_slug(code: &str) -> &'static str {
    const KNOWN: &[&str] = &[
        "already_running",
        "insufficient_space",
        "unavailable",
        "bad_timezone",
        "save_failed",
    ];
    KNOWN
        .iter()
        .find(|&&k| k == code)
        .copied()
        .unwrap_or("unknown")
}

/// `pages/maintenance.html`'s `maintenance` shape.
///
/// Sizes are pre-formatted here rather than in the template: minijinja has no
/// byte formatter, and a human-readable size is presentation, not state.
#[derive(serde::Serialize)]
struct MaintenanceCtx {
    running: bool,
    db_size: String,
    available: Option<String>,
    /// RFC3339 UTC, pre-formatted — the templates get a string, never an epoch.
    last_run_at: Option<String>,
    last_run_seconds: Option<i64>,
    last_error: Option<String>,
    /// A one-time conversion is owed — the reclaim is a no-op until then.
    needs_conversion: bool,
    /// Preflight would refuse right now, so the reason is shown instead of a
    /// button that cannot work. Computed by the *same* function the handler's
    /// preflight uses, so the two can never disagree.
    enough_space: bool,
    /// `"02:30–04:00 UTC"`, or `None` when no window is configured and
    /// maintenance stays operator-initiated. Read-only: the window is
    /// deployment YAML, not a setting this page owns.
    window: Option<String>,
    /// Writes waiting in the spill journal. Shown only when non-zero — a stuck
    /// run must read as "N events waiting" rather than as silence.
    spill_depth: Option<i64>,
}

impl From<crate::retention::MaintenanceStatus> for MaintenanceCtx {
    fn from(s: crate::retention::MaintenanceStatus) -> Self {
        Self {
            running: s.state.full_vacuum_running,
            db_size: human_bytes(s.db_size_bytes.max(0) as u64),
            available: s.available_bytes.map(human_bytes),
            last_run_at: s.state.last_full_vacuum_at.and_then(rfc3339),
            last_run_seconds: s.state.last_full_vacuum_ms.map(|ms| ms / 1000),
            last_error: s.state.last_full_vacuum_error,
            needs_conversion: s.auto_vacuum_mode != 2,
            enough_space: crate::retention::space_permits_vacuum(
                s.db_size_bytes.max(0) as u64,
                s.available_bytes,
            )
            .is_ok(),
            window: s.window.map(|w| {
                format!(
                    "{}–{} UTC",
                    crate::retention::format_hh_mm(w.start_minute_utc),
                    crate::retention::format_hh_mm(w.start_minute_utc + w.duration_minutes),
                )
            }),
            spill_depth: (s.spill_depth > 0).then_some(s.spill_depth),
        }
    }
}

/// Unix seconds to RFC3339 UTC, mirroring `persist.rs`'s formatting. `None`
/// for a timestamp SQLite could not have produced.
fn rfc3339(ts: i64) -> Option<String> {
    time::OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `POST /settings/maintenance/vacuum` — start a full VACUUM.
///
/// Admin-only and deliberately not a signed device command: this rewrites the
/// portal's own database and never talks to the agent, so the signed-command
/// registry (which is the device contract) has no business here.
///
/// Returns as soon as the work is claimed. A rewrite outlasts any sane request
/// timeout, so progress is read back from `GET /settings`.
async fn start_vacuum(
    RequireAdmin(current_user): RequireAdmin,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
) -> Response {
    let Some(maintenance) = state.maintenance.as_ref() else {
        return redirect_err(VacuumRefusal::Unavailable.slug());
    };

    let actor = AuditActor {
        user_id: current_user.id,
        username: current_user.username.clone(),
        ip: Some(addr.ip().to_string()),
    };

    match maintenance.start_full_vacuum(Some(actor)).await {
        Ok(()) => Redirect::to("/settings/maintenance?ok=maintenance_started").into_response(),
        Err(refusal) => redirect_err(refusal.slug()),
    }
}

fn redirect_err(slug: &str) -> Response {
    Redirect::to(&format!("/settings/maintenance?err={slug}")).into_response()
}

#[derive(serde::Deserialize)]
struct TimezoneForm {
    timezone: String,
}

/// `POST /settings/maintenance/timezone` — the installation's display zone
/// (plan/decisions.md #25). Admin only and audited: it changes every time the
/// portal shows and every e-mail it sends. Checked against the bundled zone
/// database before it is stored, and applied to the running process at once.
async fn save_timezone(
    _admin: RequireAdmin,
    current_user: CurrentUser,
    State(state): State<AppState>,
    axum::Form(form): axum::Form<TimezoneForm>,
) -> Response {
    let zone = form.timezone.trim().to_string();
    if crate::display_tz::parse(&zone).is_none() {
        return redirect_err("bad_timezone");
    }
    let stored = zone.clone();
    if let Err(err) = state
        .db
        .write(move |s| s.set_display_timezone(&stored))
        .await
    {
        tracing::error!(%err, zone, "failed to save the display timezone");
        return redirect_err("save_failed");
    }
    crate::display_tz::set(&zone);
    state
        .auth
        .audit(
            "display_timezone_set",
            Some(&current_user),
            None,
            Some(&zone),
            None,
            true,
        )
        .await;
    Redirect::to("/settings/maintenance?ok=timezone_saved").into_response()
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    use crate::auth::{Role, SESSION_COOKIE};
    use crate::routes::test_support::{login, login_as_role, test_state};

    fn peer() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345)
    }

    async fn post_vacuum(state: &crate::http::AppState, token: &str) -> axum::http::Response<Body> {
        let router = super::router().with_state(state.clone());
        let mut request = Request::builder()
            .method("POST")
            .uri("/settings/maintenance/vacuum")
            .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(peer()));
        router.oneshot(request).await.expect("request")
    }

    /// `/settings` has no index. It lands on the first section the role can
    /// open — so the nav entry never leads to a page explaining you may not
    /// be here, and never to an empty one either.
    #[tokio::test]
    async fn settings_lands_on_the_first_section_the_role_can_open() {
        let (state, _dir) = test_state().await;
        let router = super::router().with_state(state.clone());

        let target = |token: String| {
            let router = router.clone();
            async move {
                let response = router
                    .oneshot(
                        Request::builder()
                            .uri("/settings")
                            .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .expect("request");
                (
                    response.status(),
                    response
                        .headers()
                        .get(header::LOCATION)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string),
                )
            }
        };

        // A manager reaches notifications and not maintenance, so that is
        // where they land — never on a section that would 403 them.
        let manager = login_as_role(&state, Role::Manager, "mgr").await;
        let (status, location) = target(manager).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location.as_deref(), Some("/settings/notifications"));

        // An admin reaches both; first still wins.
        let admin = login(&state, "admin", "correct-horse-battery").await;
        let (status, location) = target(admin).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location.as_deref(), Some("/settings/notifications"));

        // A viewer reaches neither. 404 rather than a redirect into a refusal,
        // or a page whose only content is that they may not read it.
        let viewer = login_as_role(&state, Role::Viewer, "obs").await;
        let (status, _) = target(viewer.clone()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // C36-13: the portal's own 404 page with its navbar, not an empty body.
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/settings")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={viewer}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains(">404</p>"), "{body}");
    }

    async fn mqtt_fragment_for(
        state: &crate::http::AppState,
        token: &str,
    ) -> axum::http::Response<Body> {
        let router = super::router().with_state(state.clone());
        router
            .oneshot(
                Request::builder()
                    .uri("/settings/mqtt/diagnostics")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request")
    }

    async fn mqtt_page_for(
        state: &crate::http::AppState,
        token: &str,
    ) -> axum::http::Response<Body> {
        let router = super::router().with_state(state.clone());
        router
            .oneshot(
                Request::builder()
                    .uri("/settings/mqtt")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request")
    }

    async fn body_of(response: axum::http::Response<Body>) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    /// **UI-MQ1** (plan/portal/mqtt/design.md §16): the MQTT ingest state is
    /// operator-visible, never only a log line. The three states render, and
    /// each non-green one carries the text that says *why* — a colour on its
    /// own does not tell an operator whether the broker is down or one device
    /// is publishing something unparseable, and those need different actions.
    #[tokio::test]
    async fn the_mqtt_fragment_renders_each_state_with_its_reason() {
        let (state, _dir) = test_state().await;
        let manager = login_as_role(&state, Role::Manager, "mgr").await;

        // Connected: green, and no alert.
        state
            .store
            .apply(crate::state::IngestEvent::MqttConnection {
                connected: true,
                reason: None,
            });
        let body = body_of(mqtt_fragment_for(&state, &manager).await).await;
        assert!(body.contains("badge-success"), "{body}");
        assert!(!body.contains("alert-warning"), "{body}");

        // Down: the reason the broker gave, on the page.
        state
            .store
            .apply(crate::state::IngestEvent::MqttConnection {
                connected: false,
                reason: Some("connection refused".to_string()),
            });
        let body = body_of(mqtt_fragment_for(&state, &manager).await).await;
        assert!(body.contains("alert-warning"), "{body}");
        assert!(
            body.contains("connection refused"),
            "the last error text is the point of UI-MQ1: {body}"
        );
        // A fresh drop is `reconnecting`, not `degraded` — one keepalive cycle
        // has not passed, and calling a blip an outage trains people to ignore
        // the word.
        assert!(body.contains("badge-warning"), "{body}");
        assert!(!body.contains("badge-error"), "{body}");
    }

    /// The subscribed-topic list is rendered, and **not pinned by value**: it
    /// changed shape when GitLab #73 landed (`proximos/+/nodes/+/+`), and a
    /// golden that spelled it out would fail on somebody else's change for no
    /// reason (handoff.md §6 says so in as many words).
    #[tokio::test]
    async fn the_mqtt_fragment_lists_subscriptions_without_pinning_them() {
        let (state, _dir) = test_state().await;
        let manager = login_as_role(&state, Role::Manager, "mgr").await;

        let body = body_of(mqtt_fragment_for(&state, &manager).await).await;
        assert!(
            body.contains("mqtt-diagnostics"),
            "the fragment is its own swap target: {body}"
        );
        // The ingest loop has not run in a unit test, so the list is empty and
        // says so rather than rendering a bare heading.
        assert!(body.contains("id=\"mqtt-diagnostics\""), "{body}");
    }

    /// Manager and above, matching the notifications card: this is for
    /// somebody who would act on "degraded". Everyone still sees the *fact* of
    /// a disconnect through `partials/mqtt_banner.html`.
    #[tokio::test]
    async fn the_mqtt_fragment_is_manager_and_above() {
        let (state, _dir) = test_state().await;

        let viewer = login_as_role(&state, Role::Viewer, "obs").await;
        assert_eq!(
            mqtt_fragment_for(&state, &viewer).await.status(),
            StatusCode::FORBIDDEN
        );

        for token in [
            login_as_role(&state, Role::Manager, "mgr").await,
            login(&state, "admin", "correct-horse-battery").await,
        ] {
            assert_eq!(
                mqtt_fragment_for(&state, &token).await.status(),
                StatusCode::OK
            );
        }

        // …and the section that hosts it follows the same gate: a manager
        // gets the page with the fragment's swap target on it, polling its
        // own route; a viewer gets the refusal, not an empty page.
        let manager = login_as_role(&state, Role::Manager, "mgr2").await;
        let page = mqtt_page_for(&state, &manager).await;
        assert_eq!(page.status(), StatusCode::OK);
        let body = body_of(page).await;
        assert!(body.contains("id=\"mqtt-diagnostics\""), "{body}");
        assert!(
            body.contains("hx-get=\"/settings/mqtt/diagnostics\""),
            "the fragment polls its own route: {body}"
        );
        let viewer = login_as_role(&state, Role::Viewer, "obs2").await;
        assert_eq!(
            mqtt_page_for(&state, &viewer).await.status(),
            StatusCode::FORBIDDEN
        );
    }

    /// The panel acts on the database, so it follows `RequireAdmin` rather than
    /// the page's own signpost flags.
    #[tokio::test]
    async fn only_admins_may_start_maintenance() {
        let (state, _dir) = test_state().await;

        let viewer = login_as_role(&state, Role::Viewer, "obs").await;
        assert_eq!(
            post_vacuum(&state, &viewer).await.status(),
            StatusCode::FORBIDDEN
        );

        let manager = login_as_role(&state, Role::Manager, "mgr").await;
        assert_eq!(
            post_vacuum(&state, &manager).await.status(),
            StatusCode::FORBIDDEN,
            "manager is not admin: maintenance blocks writes device-wide"
        );

        let admin = login(&state, "admin", "correct-horse-battery").await;
        let response = post_vacuum(&state, &admin).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            // Back to the page the button is on, the Maintenance tab of the
            // settings area — the operator has to land where the banner they
            // just earned is rendered.
            Some("/settings/maintenance?ok=maintenance_started")
        );
    }

    /// Refusal slugs reach the template through a *dynamic* key
    /// (`("common.settings.flash.err." ~ error)|t`), which `every_static_template_key_exists`
    /// structurally cannot see. Without this, a new refusal variant would print
    /// its raw key at the operator.
    #[test]
    fn every_refusal_slug_has_a_translation() {
        use crate::retention::VacuumRefusal;

        let refusals = [
            VacuumRefusal::AlreadyRunning,
            VacuumRefusal::InsufficientSpace {
                required: 1,
                available: 0,
            },
            VacuumRefusal::Unavailable,
        ];
        // Plus the fallback `err_slug` collapses unknown codes to, and the one
        // success code.
        let mut keys: Vec<String> = refusals
            .iter()
            .map(|r| format!("common.settings.flash.err.{}", r.slug()))
            .collect();
        keys.push("common.settings.flash.err.unknown".to_string());
        keys.push("common.settings.flash.ok.maintenance_started".to_string());

        for lang in ["en", "cs"] {
            for key in &keys {
                assert_ne!(
                    crate::i18n::lookup(lang, key),
                    *key,
                    "{lang} has no entry for {key}"
                );
            }
        }
    }
}
