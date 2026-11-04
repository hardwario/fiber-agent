//! Admin user-management routes (`/settings/users*`, `RequireAdmin`-gated).
//!
//! Three pages. `/settings/users` is the list, `/settings/users/new` the
//! create form (a page of its own since 2026-10-15, like `/nodes/add`; a
//! plain form POST that redirects to the new user's page or re-renders the
//! form with the error). `/settings/users/{id}` is the user's own page, and edit, enable, disable
//! and reset-password re-render `partials/user_detail.html` — the card the
//! operator is looking at — while delete answers `HX-Redirect` back to the
//! list, since the page it was fired from no longer exists.
//!
//! Every mutation answers HTTP 200 even when it failed, with the reason in
//! the fragment's `error` slot: htmx skips the swap on non-2xx, which would
//! leave a rejected action with no visible feedback at all.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use minijinja::context;
use portal_storage::User;
use serde::{Deserialize, Serialize};

use crate::auth::{CurrentUser, NewUserInput, RequireAdmin, Role};
use crate::http::AppState;
use crate::render;

use super::{non_empty, render as render_page, render_fragment};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings/users", get(users_page).post(create_user))
        .route("/settings/users/new", get(new_user_page))
        .route("/settings/users/{id}", get(user_detail))
        .route("/settings/users/{id}/edit", post(edit_user))
        .route("/settings/users/{id}/enable", post(enable_user))
        .route("/settings/users/{id}/disable", post(disable_user))
        .route("/settings/users/{id}/delete", post(delete_user))
        .route("/settings/users/{id}/reset-password", post(reset_password))
}

/// `partials/user_row.html`'s `u` shape.
#[derive(Serialize)]
struct UserRowCtx {
    id: i64,
    username: String,
    full_name: Option<String>,
    job_title: Option<String>,
    user_role: String,
    is_superuser: bool,
    enabled: bool,
    created_at: String,
    last_login: Option<String>,
}

impl From<&User> for UserRowCtx {
    fn from(u: &User) -> Self {
        Self {
            id: u.id,
            username: u.username.clone(),
            full_name: u.full_name.clone(),
            job_title: u.job_title.clone(),
            user_role: u.user_role.clone(),
            is_superuser: u.is_superuser,
            enabled: u.enabled,
            created_at: u.created_at.clone(),
            last_login: u.last_login.clone(),
        }
    }
}

#[derive(Deserialize)]
struct CreateUserForm {
    username: String,
    #[serde(default)]
    full_name: String,
    password: String,
    user_role: String,
}

/// `user_role` is absent when the dialog rendered no select (your own
/// account, a superuser, an admin seen by a non-superuser); `full_name` is
/// absent only from a hand-made request, and then the name is kept.
#[derive(Deserialize)]
struct EditUserForm {
    full_name: Option<String>,
    user_role: Option<String>,
}

#[derive(Deserialize)]
struct ResetPasswordForm {
    password: String,
}

/// `pages/user_add.html`, empty or carrying back a refused create: the
/// typed username, name and role come back (never the password) with the
/// `auth.errors.*` slug in `error`.
async fn render_new_user_page(
    state: &AppState,
    current_user: &CurrentUser,
    form: Option<&CreateUserForm>,
    error: Option<String>,
) -> Response {
    let users = state.auth.list_users(true).await.unwrap_or_default();
    let stats = render::user_stats_ctx(state.store.snapshot_mqtt().connected, &users);
    let ctx = render::page_ctx(
        context! {
            nav_active => "users",
            stats => stats,
            error => error,
            username => form.map(|f| f.username.clone()),
            full_name => form.map(|f| f.full_name.clone()),
            user_role => form.map(|f| f.user_role.clone()).unwrap_or_else(|| "viewer".into()),
        },
        Some(current_user),
    );
    render_page(state, "pages/user_add.html", ctx)
}

/// GET /settings/users/new — the create form as a page.
async fn new_user_page(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
) -> Response {
    render_new_user_page(&state, &current_user, None, None).await
}

/// Re-renders one user's card (`partials/user_detail.html`) — the response
/// to every action on `pages/user_detail.html`, success or failure alike.
/// A user that vanished under the action (deleted by someone else) sends
/// the browser back to the list rather than swapping in an empty card.
///
/// `ok` names the action that succeeded (`auth.users.ok_<ok>`), for an
/// action whose result the card does not show by itself: a reset password
/// looks exactly like the old one.
async fn render_user_card(
    state: &AppState,
    current_user: &CurrentUser,
    id: i64,
    error: Option<String>,
    ok: Option<&str>,
) -> Response {
    match state.auth.get_user(id).await {
        Ok(Some(user)) => {
            let ctx = render::page_ctx(
                context! { u => UserRowCtx::from(&user), error => error, ok => ok },
                Some(current_user),
            );
            render_fragment(state, "partials/user_detail.html", ctx)
        }
        Ok(None) => redirect_to_list(),
        Err(err) => {
            tracing::error!(%err, id, "failed to load user");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to load user").into_response()
        }
    }
}

/// `HX-Redirect`, not a 303: these actions are htmx POSTs, and a redirect
/// status would only make htmx swap the list page's HTML into the card.
fn redirect_to_list() -> Response {
    hx_redirect("/settings/users")
}

fn hx_redirect(location: &str) -> Response {
    let mut response = StatusCode::OK.into_response();
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert("HX-Redirect", value);
    }
    response
}

/// `GET /settings/users?deleted=<username>`: the flash a delete lands on.
#[derive(Deserialize, Default)]
#[serde(default)]
struct UsersQuery {
    deleted: Option<String>,
}

/// `GET /settings/users/{id}?created=1`: the flash a create lands on.
#[derive(Deserialize, Default)]
#[serde(default)]
struct DetailQuery {
    created: Option<String>,
}

async fn users_page(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Query(q): Query<UsersQuery>,
) -> Response {
    match state.auth.list_users(true).await {
        Ok(users) => {
            let rows: Vec<UserRowCtx> = users.iter().map(UserRowCtx::from).collect();
            // The metric row, from the list already in hand — `routes::render`
            // builds no tiles for this page (`StatsScope::None`).
            let stats = render::user_stats_ctx(state.store.snapshot_mqtt().connected, &users);
            let ctx = render::page_ctx(
                context! {
                    nav_active => "users",
                    stats => stats,
                    users => rows,
                    error => Option::<String>::None,
                    deleted => q.deleted.and_then(non_empty),
                },
                Some(&current_user),
            );
            render_page(&state, "pages/users.html", ctx)
        }
        Err(err) => {
            tracing::error!(%err, "failed to list users");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to list users").into_response()
        }
    }
}

/// GET /settings/users/{id} — one user's page (`pages/user_detail.html`),
/// opened by a row of the list. An id that resolves to nothing is a 404
/// like any other unknown URL: it is a page now, not a panel, so there is
/// nothing for an empty state to sit in.
async fn user_detail(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<DetailQuery>,
) -> Response {
    match state.auth.get_user(id).await {
        Ok(Some(user)) => {
            // The list's tile row on top, like the Node/Beacon/Hub detail
            // pages carry the row of the list they came from.
            let users = state.auth.list_users(true).await.unwrap_or_default();
            let stats = render::user_stats_ctx(state.store.snapshot_mqtt().connected, &users);
            let ctx = render::page_ctx(
                context! {
                    nav_active => "users",
                    stats => stats,
                    u => UserRowCtx::from(&user),
                    ok => q.created.is_some().then_some("created"),
                },
                Some(&current_user),
            );
            render_page(&state, "pages/user_detail.html", ctx)
        }
        Ok(None) => super::not_found(&state, Some(&current_user)),
        Err(err) => {
            tracing::error!(%err, id, "failed to load user");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to load user").into_response()
        }
    }
}

async fn create_user(
    RequireAdmin(actor): RequireAdmin,
    State(state): State<AppState>,
    Form(form): Form<CreateUserForm>,
) -> Response {
    let role: Role = match form.user_role.parse() {
        Ok(role) => role,
        Err(_) => {
            return render_new_user_page(&state, &actor, Some(&form), Some("invalid_role".into()))
                .await
        }
    };
    let input = NewUserInput {
        username: form.username.clone(),
        password: form.password.clone(),
        full_name: non_empty(form.full_name.clone()),
        job_title: None,
        user_role: role,
    };
    match state.auth.create_user(&actor, input).await {
        Ok(user) => Redirect::to(&format!("/settings/users/{}?created=1", user.id)).into_response(),
        Err(e) => {
            render_new_user_page(&state, &actor, Some(&form), Some(e.slug().to_string())).await
        }
    }
}

/// LE-13 (local E2E 2026-09-26): a refused account action (a guard: disabling oneself, an
/// admin touching another admin) left no audit row. A guard refusal is an attempt someone made,
/// so it is recorded as failed with its reason; a plain validation slip is not.
async fn audit_refusal(
    state: &AppState,
    actor: &crate::auth::CurrentUser,
    event: &str,
    id: i64,
    err: &crate::auth::AuthError,
) {
    if matches!(err, crate::auth::AuthError::SelfGuard(_)) {
        let details = serde_json::json!({ "refused": err.slug(), "user_id": id }).to_string();
        // The username as the target, like the successful rows, so one filter finds both
        // (R-10, review of !102).
        let target = match state.auth.get_user(id).await {
            Ok(Some(user)) => user.username,
            _ => id.to_string(),
        };
        state
            .auth
            .audit(
                event,
                Some(actor),
                None,
                Some(&target),
                Some(details),
                false,
            )
            .await;
    }
}

/// POST /settings/users/{id}/edit — full name and role
/// (plan/portal/user_admin.md). The guards and the audit live in
/// `AuthService::update_user`.
async fn edit_user(
    RequireAdmin(actor): RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<EditUserForm>,
) -> Response {
    let error = state
        .auth
        .update_user(
            &actor,
            id,
            form.full_name.as_deref(),
            form.user_role.as_deref(),
        )
        .await
        .err()
        .map(|e| e.slug().to_string());
    let ok = error.is_none().then_some("updated");
    render_user_card(&state, &actor, id, error, ok).await
}

async fn enable_user(
    RequireAdmin(actor): RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    let error = match state.auth.set_enabled(&actor, id, true).await {
        Ok(_) => None,
        Err(e) => {
            audit_refusal(&state, &actor, "user_enable", id, &e).await;
            Some(e.slug().to_string())
        }
    };
    render_user_card(&state, &actor, id, error, None).await
}

async fn disable_user(
    RequireAdmin(actor): RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    let error = match state.auth.set_enabled(&actor, id, false).await {
        Ok(_) => None,
        Err(e) => {
            audit_refusal(&state, &actor, "user_disable", id, &e).await;
            Some(e.slug().to_string())
        }
    };
    render_user_card(&state, &actor, id, error, None).await
}

async fn delete_user(
    RequireAdmin(actor): RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    // Read before the row is gone: the flash names who was deleted.
    let username = match state.auth.get_user(id).await {
        Ok(Some(user)) => user.username,
        _ => String::new(),
    };
    match state.auth.delete_user(&actor, id).await {
        Ok(()) => hx_redirect(&format!(
            "/settings/users?deleted={}",
            super::percent_encode_query(&username)
        )),
        Err(err) => {
            audit_refusal(&state, &actor, "user_delete", id, &err).await;
            render_user_card(&state, &actor, id, Some(err.slug().to_string()), None).await
        }
    }
}

async fn reset_password(
    RequireAdmin(actor): RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<ResetPasswordForm>,
) -> Response {
    let error = match state.auth.reset_password(&actor, id, &form.password).await {
        Ok(()) => None,
        Err(e) => {
            audit_refusal(&state, &actor, "password_reset", id, &e).await;
            Some(e.slug().to_string())
        }
    };
    let ok = error.is_none().then_some("reset_password");
    render_user_card(&state, &actor, id, error, ok).await
}

/// Smoke coverage for the pages/partials this milestone rewired to
/// translate `error` slugs instead of English messages — `user_routes.rs`
/// had no tests before M9.
#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    use crate::auth::{Role, SESSION_COOKIE};
    use crate::routes::test_support::{login, login_as_role, test_state};

    async fn body_text(response: axum::response::Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn users_page_renders_for_admin() {
        let (state, _dir) = test_state().await;
        let token = login(&state, "admin", "correct-horse-battery").await;
        let router = super::router().with_state(state);

        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/settings/users/new")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;
        assert!(body.contains("<html"), "the create form is a page");
        assert!(body.contains(r#"action="/settings/users""#));
        let form = &body[body.find("name=\"user_role\"").expect("create form")..];
        let form = &form[..form.find("</select>").expect("select end")];
        assert!(
            form.contains("<option value=\"admin\""),
            "the superuser is offered the admin role"
        );
    }

    /// The list wears the shared toolbar and its rows open a page, which is
    /// the whole point of the rewrite — and both are template-only facts, so
    /// nothing else would catch a macro call or a row link going wrong.
    #[tokio::test]
    async fn users_page_wears_the_toolbar_and_links_to_the_detail_page() {
        let (state, _dir) = test_state().await;
        let token = login(&state, "admin", "correct-horse-battery").await;
        let router = super::router().with_state(state);

        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/settings/users")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;
        assert!(
            body.contains(r#"data-list-key="users""#),
            "the script's root"
        );
        assert!(
            body.contains("data-list-filters-toggle"),
            "the Filters button"
        );
        for field in ["role", "status"] {
            assert!(
                body.contains(&format!(r#"data-list-filter="{field}""#)),
                "the {field} filter is wired to the client-side pass"
            );
        }
        assert!(
            body.contains(r#"href="/settings/users/1""#),
            "the row opens the user's own page"
        );
        assert!(
            body.contains("&#x2f;settings&#x2f;users&#x2f;new"),
            "+ Add opens the create page, not a dialog"
        );
        for href in ["?status=enabled", "?role=admin", "?status=disabled"] {
            assert!(
                body.contains(&format!("&#x2f;settings&#x2f;users{href}")),
                "a tile filters the list by {href}"
            );
        }
    }

    #[tokio::test]
    async fn user_detail_is_a_page_and_an_unknown_id_is_a_404() {
        let (state, _dir) = test_state().await;
        let token = login(&state, "admin", "correct-horse-battery").await;
        let router = super::router().with_state(state);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/settings/users/1")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;
        assert!(body.contains("<html"), "a full page, not the card fragment");
        assert!(body.contains(r#"id="user-detail""#), "with the card in it");
        assert!(
            body.contains("&#x2f;settings&#x2f;users?role=admin"),
            "the list's tile row sits on top"
        );

        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/settings/users/9999")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn create_user_with_invalid_role_renders_translated_error() {
        let (state, _dir) = test_state().await;
        let token = login(&state, "admin", "correct-horse-battery").await;
        let router = super::router().with_state(state);

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/settings/users")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(
                        "username=newbie&full_name=&password=password123&user_role=overlord",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;
        assert!(!body.contains("invalid_role"));
        assert!(body.contains("valid role"));
    }

    /// A create lands on the new user's page with a flash, and that page's
    /// tiles count the new user.
    #[tokio::test]
    async fn creating_a_user_redirects_to_its_page() {
        let (state, _dir) = test_state().await;
        let token = login(&state, "admin", "correct-horse-battery").await;
        let router = super::router().with_state(state);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/settings/users")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(
                        "username=newbie&full_name=&password=password123&user_role=viewer",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(location, "/settings/users/2?created=1");

        let response = router
            .oneshot(
                Request::builder()
                    .uri(location)
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_text(response).await;
        assert!(body.contains("User newbie created."), "{body}");
        let tiles = &body[body.find(r#"id="page-stats""#).expect("tiles")..];
        assert!(tiles.contains(">2<"), "two users now");
    }

    /// UX-051: a password reset and a delete both say they happened, and
    /// the destructive actions confirm in the portal's dialog, not in the
    /// browser's `confirm()`.
    #[tokio::test]
    async fn reset_and_delete_say_what_they_did() {
        let (state, _dir) = test_state().await;
        let token = login(&state, "admin", "correct-horse-battery").await;
        let router = super::router().with_state(state);
        let send = |method: &str, uri: &str, body: &'static str| {
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap()
        };

        router
            .clone()
            .oneshot(send(
                "POST",
                "/settings/users",
                "username=newbie&full_name=&password=password123&user_role=viewer",
            ))
            .await
            .unwrap();

        let card = body_text(
            router
                .clone()
                .oneshot(send("GET", "/settings/users/2", ""))
                .await
                .unwrap(),
        )
        .await;
        assert!(!card.contains("hx-confirm"), "no native confirm left");
        assert!(
            card.contains(r#"id="user-delete-2""#),
            "delete confirms in a dialog"
        );
        assert!(
            card.contains(r#"id="user-disable-2""#),
            "so does deactivate"
        );

        let reset = body_text(
            router
                .clone()
                .oneshot(send(
                    "POST",
                    "/settings/users/2/reset-password",
                    "password=another-password-1",
                ))
                .await
                .unwrap(),
        )
        .await;
        assert!(reset.contains("Password for newbie changed"), "{reset}");

        let deleted = router
            .clone()
            .oneshot(send("POST", "/settings/users/2/delete", ""))
            .await
            .unwrap();
        assert_eq!(
            deleted.headers().get("HX-Redirect").unwrap(),
            "/settings/users?deleted=newbie"
        );
        let list = body_text(
            router
                .oneshot(send("GET", "/settings/users?deleted=newbie", ""))
                .await
                .unwrap(),
        )
        .await;
        assert!(list.contains("User newbie deleted."), "the list flashes it");
    }

    fn send(method: &str, uri: &str, token: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// user_admin.md §2: the role select is rendered only where the role may
    /// change; the admin's own page and a superuser's page say why instead.
    #[tokio::test]
    async fn the_role_select_is_absent_on_your_own_and_a_superusers_page() {
        let (state, _dir) = test_state().await;
        let root = login(&state, "admin", "correct-horse-battery").await;
        login_as_role(&state, Role::Manager, "mia").await;
        let router = super::router().with_state(state);
        let page = |id: i64| {
            let router = router.clone();
            let root = root.clone();
            async move {
                let uri = format!("/settings/users/{id}");
                body_text(router.oneshot(send("GET", &uri, &root, "")).await.unwrap()).await
            }
        };

        let own = page(1).await;
        assert!(own.contains(r#"id="user-edit-1""#), "the dialog is there");
        assert!(!own.contains(r#"name="user_role""#), "{own}");
        assert!(own.contains("Your own role cannot be changed"), "{own}");

        let manager = page(2).await;
        assert!(manager.contains(r#"name="user_role""#));
        assert!(manager.contains(r#"name="full_name""#));
        // The full name is an inline edit block (ui_components.md §1),
        // the dialog keeps only the role.
        assert!(
            manager.contains(r#"id="user-full-name-2" data-te-in"#),
            "{manager}"
        );
        assert!(manager.contains("te-pencil"), "{manager}");
        assert_eq!(
            manager.matches(r#"name="full_name""#).count(),
            1,
            "{manager}"
        );
        assert!(!manager.contains("job title"), "job_title left the card");
        assert!(!manager.contains("Job title"), "job_title left the card");
    }

    /// The route: a saved edit re-renders the card with the ok line, and a
    /// guard's refusal comes back as the translated slug, both HTTP 200.
    #[tokio::test]
    async fn edit_saves_and_refusals_render_in_the_card() {
        let (state, _dir) = test_state().await;
        let root = login(&state, "admin", "correct-horse-battery").await;
        login_as_role(&state, Role::Manager, "mia").await;
        let router = super::router().with_state(state.clone());

        let ok = router
            .clone()
            .oneshot(send(
                "POST",
                "/settings/users/2/edit",
                &root,
                "full_name=Mia+Nov%C3%A1&user_role=viewer",
            ))
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let ok = body_text(ok).await;
        assert!(ok.contains("User mia updated."), "{ok}");
        assert!(ok.contains("Mia Nová"));
        let mia = state.auth.get_user(2).await.unwrap().unwrap();
        assert_eq!(mia.user_role, "viewer");

        let refused = router
            .oneshot(send(
                "POST",
                "/settings/users/1/edit",
                &root,
                "full_name=&user_role=viewer",
            ))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::OK);
        let refused = body_text(refused).await;
        assert!(refused.contains("alert-error"), "{refused}");
        assert!(!refused.contains("self_guard_role_change_self"));
    }

    /// §7: a manager gets 403 on `/edit`.
    #[tokio::test]
    async fn a_manager_is_refused_the_edit_route() {
        let (state, _dir) = test_state().await;
        let manager = login_as_role(&state, Role::Manager, "mia").await;
        let router = super::router().with_state(state.clone());

        let response = router
            .oneshot(send(
                "POST",
                "/settings/users/2/edit",
                &manager,
                "full_name=&user_role=admin",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            state.auth.get_user(2).await.unwrap().unwrap().user_role,
            "manager"
        );
    }

    /// §5: a manager demoted to viewer gets 403 on a manager route with the
    /// same cookie; the session itself was not deleted.
    #[tokio::test]
    async fn a_demoted_manager_is_refused_with_the_same_session() {
        use axum::routing::post;

        use crate::auth::RequireManager;

        let (state, _dir) = test_state().await;
        let root = login(&state, "admin", "correct-horse-battery").await;
        let manager = login_as_role(&state, Role::Manager, "mia").await;
        let manager_route = axum::Router::new()
            .route("/manager-only", post(|_: RequireManager| async { "ok" }))
            .with_state(state.clone());

        let before = manager_route
            .clone()
            .oneshot(send("POST", "/manager-only", &manager, ""))
            .await
            .unwrap();
        assert_eq!(before.status(), StatusCode::OK);

        super::router()
            .with_state(state.clone())
            .oneshot(send(
                "POST",
                "/settings/users/2/edit",
                &root,
                "full_name=&user_role=viewer",
            ))
            .await
            .unwrap();

        let after = manager_route
            .oneshot(send("POST", "/manager-only", &manager, ""))
            .await
            .unwrap();
        assert_eq!(after.status(), StatusCode::FORBIDDEN);
        let session = state.auth.validate(&manager).await.expect("session kept");
        assert_eq!(session.user_role, Role::Viewer);
    }

    /// LE-6: a role-admin must not reach the bootstrap account: the card
    /// hides the actions, and a forged POST is refused with the translated
    /// reason and leaves the superuser's password as it was.
    #[tokio::test]
    async fn a_role_admin_cannot_reset_the_superusers_password() {
        let (state, _dir) = test_state().await;
        let token = login_as_role(&state, Role::Admin, "alice").await;
        let router = super::router().with_state(state.clone());

        let response = router
            .clone()
            .oneshot(send("GET", "/settings/users/1", &token, ""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;
        for action in ["reset-password", "disable", "delete"] {
            assert!(
                !body.contains(&format!("/settings/users/1/{action}")),
                "no {action} control on the superuser's card"
            );
        }

        let response = router
            .oneshot(send(
                "POST",
                "/settings/users/1/reset-password",
                &token,
                "password=takeover-1234",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;
        assert!(
            body.contains("Only a Superuser can change a Superuser or an Admin account."),
            "{body}"
        );

        state
            .auth
            .login("admin".into(), "correct-horse-battery".into(), None)
            .await
            .expect("the superuser's password is unchanged");
    }
}
