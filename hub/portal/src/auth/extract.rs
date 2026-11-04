//! `CurrentUser`/`RequireManager`/`RequireAdmin` axum extractors, backed by
//! the session cookie + [`AuthService::validate`].

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::CookieJar;

use super::service::AuthService;
use super::types::{CurrentUser, Role};

/// Cookie name carrying the session token (see `auth/cookies.rs`).
pub const SESSION_COOKIE: &str = "portal_session";

/// Rejection for a missing/invalid/expired session. Browser navigations get
/// a 303 to `/login`; htmx requests (`HX-Request` header present) also get
/// `HX-Redirect` so htmx follows it instead of swapping the redirect body
/// into the target element. A plain GET carries its path and query as
/// `?next=`, so a link from an alarm e-mail lands on the alarm after login
/// (UX-027); the login POST decides whether `next` is safe to follow.
pub struct AuthRedirect {
    hx_request: bool,
    next: Option<String>,
}

impl AuthRedirect {
    fn from_parts(parts: &Parts) -> Self {
        let hx_request = parts.headers.contains_key("HX-Request");
        let next = (!hx_request && parts.method == axum::http::Method::GET)
            .then(|| parts.uri.path_and_query().map(|pq| pq.as_str().to_string()))
            .flatten()
            .filter(|pq| pq != "/");
        Self { hx_request, next }
    }
}

impl IntoResponse for AuthRedirect {
    fn into_response(self) -> Response {
        let target = match &self.next {
            Some(next) => format!("/login?next={}", crate::routes::percent_encode_query(next)),
            None => "/login".to_string(),
        };
        let mut response = Redirect::to(&target).into_response();
        if self.hx_request {
            response
                .headers_mut()
                .insert("HX-Redirect", HeaderValue::from_static("/login"));
        }
        response
    }
}

impl<S> FromRequestParts<S> for CurrentUser
where
    S: Send + Sync,
    AuthService: FromRef<S>,
{
    type Rejection = AuthRedirect;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let reject = || AuthRedirect::from_parts(parts);

        let jar = CookieJar::from_headers(&parts.headers);
        let token = jar
            .get(SESSION_COOKIE)
            .map(|c| c.value().to_string())
            .ok_or_else(reject)?;

        let auth = AuthService::from_ref(state);
        auth.validate(&token).await.ok_or_else(reject)
    }
}

/// `Option<CurrentUser>` for the handful of pages that render for anyone but
/// render *better* when they know who is asking — the 404 fallback, which keeps
/// the navbar for a logged-in visitor and shows a bare brand link otherwise
/// (`templates/layout/nav.html` handles both). axum 0.8 routes `Option<T>`
/// through its own trait, so this cannot fall out of the impl above; it just
/// discards the redirect that impl would have returned.
impl<S> axum::extract::OptionalFromRequestParts<S> for CurrentUser
where
    S: Send + Sync,
    AuthService: FromRef<S>,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(
            <CurrentUser as FromRequestParts<S>>::from_request_parts(parts, state)
                .await
                .ok(),
        )
    }
}

/// Rejection shared by every role-gated extractor below.
///
/// Unauthenticated requests still redirect to `/login`;
/// authenticated-but-under-ranked requests get a plain 403 body (there is
/// nowhere sensible to redirect an already-logged-in user).
///
/// `Forbidden` carries the **i18n key** for its message rather than the
/// message itself: the extractor runs after `i18n::lang_middleware` has set
/// the per-request task-local, so the lookup is valid here, and carrying the
/// key keeps the 403 body translated the way the hand-rolled
/// `routes::forbidden` it replaced already was (two of its thirteen
/// callers had hardcoded English, which this fixes in passing). Resolved in `into_response` so the
/// key travels as a `&'static str` and nothing allocates on the happy path.
pub enum RoleRejection {
    Unauthenticated(AuthRedirect),
    /// The message key, and the user who was refused (for the page's nav).
    Forbidden(&'static str, Box<CurrentUser>),
}

/// Rides on a role 403 so `http::forbidden_page` can render it inside the
/// normal layout, with the refused user's navigation and a way back. The
/// bare body stays underneath for htmx requests and for any path the
/// middleware does not wrap.
#[derive(Clone)]
pub struct ForbiddenPage {
    pub message: String,
    pub user: CurrentUser,
}

/// The pre-WP0.1 name, kept so `auth`'s public surface does not churn.
pub type RequireAdminRejection = RoleRejection;

impl IntoResponse for RoleRejection {
    fn into_response(self) -> Response {
        match self {
            RoleRejection::Unauthenticated(redirect) => redirect.into_response(),
            RoleRejection::Forbidden(key, user) => {
                let message = crate::i18n::lookup(crate::i18n::current_lang(), key);
                let mut response = (
                    StatusCode::FORBIDDEN,
                    Html(format!("<h1>403 Forbidden</h1><p>{message}</p>")),
                )
                    .into_response();
                response.extensions_mut().insert(ForbiddenPage {
                    message,
                    user: *user,
                });
                response
            }
        }
    }
}

/// Shared body of every role-gated extractor: resolve the session, then
/// check the rank. `min` is folded through [`CurrentUser::has_role`], so
/// `is_superuser` clears every bar — see that method for why the two axes
/// are never compared separately.
async fn require_role<S>(
    parts: &mut Parts,
    state: &S,
    min: Role,
    forbidden_key: &'static str,
) -> Result<CurrentUser, RoleRejection>
where
    S: Send + Sync,
    AuthService: FromRef<S>,
{
    let user = CurrentUser::from_request_parts(parts, state)
        .await
        .map_err(RoleRejection::Unauthenticated)?;
    if user.has_role(min) {
        return Ok(user);
    }
    // decisions.md #28e: every role refusal comes through here, so this is
    // the one audit write for it — a failed row naming the user, the route
    // and the rank it needed.
    let ip = parts
        .extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip().to_string());
    let target = format!("{} {}", parts.method, parts.uri.path());
    AuthService::from_ref(state)
        .audit(
            "permission_denied",
            Some(&user),
            ip.as_deref(),
            Some(&target),
            Some(serde_json::json!({ "required": min.as_str() }).to_string()),
            false,
        )
        .await;
    Err(RoleRejection::Forbidden(forbidden_key, Box::new(user)))
}

/// Like [`CurrentUser`], but additionally requires
/// [`CurrentUser::is_admin`].
pub struct RequireAdmin(pub CurrentUser);

impl<S> FromRequestParts<S> for RequireAdmin
where
    S: Send + Sync,
    AuthService: FromRef<S>,
{
    type Rejection = RoleRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        require_role(
            parts,
            state,
            Role::Admin,
            "monitoring.shared.admin_required",
        )
        .await
        .map(RequireAdmin)
    }
}

/// Like [`CurrentUser`], but additionally requires manager-or-better.
///
/// The server-side half of the gate `render::can_edit_thresholds` renders:
/// WP0.1 (devices.backend.md §3) exists because a template that hides a
/// control is not an authorization check. Before it, manager+ routes each
/// carried their own `if !is_manager_or_admin(..) { return forbidden(..) }`
/// prologue — thirteen copies, and the one route that
/// forgot the prologue was reachable by any signed-in viewer.
pub struct RequireManager(pub CurrentUser);

impl<S> FromRequestParts<S> for RequireManager
where
    S: Send + Sync,
    AuthService: FromRef<S>,
{
    type Rejection = RoleRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        require_role(
            parts,
            state,
            Role::Manager,
            "monitoring.shared.manager_or_admin_required",
        )
        .await
        .map(RequireManager)
    }
}
