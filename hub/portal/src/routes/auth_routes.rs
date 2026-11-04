//! Session lifecycle routes: first-run setup, login/logout, and the
//! logged-in user's own password change. Context contracts: see
//! `templates/pages/{setup,login,password}.html` header comments.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::cookie::CookieJar;
use minijinja::context;
use serde::Deserialize;

use crate::auth::{self, CurrentUser};
use crate::http::AppState;
use crate::render;

use super::non_empty;
use super::render as render_page;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/setup", get(setup_page).post(setup_post))
        .route("/login", get(login_page).post(login_post))
        .route("/logout", post(logout))
        .route("/settings/password", get(password_page).post(password_post))
}

#[derive(Deserialize)]
struct SetupForm {
    username: String,
    #[serde(default)]
    full_name: String,
    password: String,
    password_confirm: String,
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
    /// Where the login came from (`auth/extract.rs`), echoed by the form.
    #[serde(default)]
    next: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LoginQuery {
    next: Option<String>,
}

/// Where a successful login goes: `next` when it is a path on this portal
/// (starts with `/`, not `//`, no backslash or control character, any of
/// which a browser may read as another host), else `/`. The open-redirect
/// guard of plan/portal/email_links.md §3.
pub(crate) fn safe_next(next: Option<&str>) -> &str {
    next.filter(|n| {
        n.starts_with('/')
            && !n.starts_with("//")
            && !n.contains('\\')
            && !n.chars().any(char::is_control)
    })
    .unwrap_or("/")
}

#[derive(Deserialize)]
struct PasswordForm {
    old: String,
    new: String,
    confirm: String,
}

/// Client IP for the audit log and the attempt throttle, see [`auth::client_ip`].
fn client_ip(ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: &HeaderMap) -> Option<String> {
    Some(auth::client_ip(addr, headers))
}

/// What is left of the lockout, for a client that reads the header rather than the page
/// (S37-9, #103), in whole seconds rounded up.
fn retry_after(mut response: Response, err: &auth::AuthError) -> Response {
    if let auth::AuthError::TooManyAttempts { retry_after } = err {
        let secs = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
        response.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from(secs.max(1)),
        );
    }
    response
}

async fn setup_page(State(state): State<AppState>) -> Response {
    if !state.auth.setup_required().await {
        return Redirect::to("/login").into_response();
    }
    setup_form(&state, None)
}

/// `pages/setup.html`. The radio mode is not asked here: it is chosen at provisioning,
/// from the Manager app over BLE (decision #44, GitLab #154).
fn setup_form(state: &AppState, error: Option<&str>) -> Response {
    render_page(
        state,
        "pages/setup.html",
        render::page_ctx(context! { error => error }, None),
    )
}

/// Where a finished `/setup` sends the operator: the Hub's pairing card (System tab, through
/// the `/device?tab=configuration` redirect — or `/hubs`' waiting state before the Hub has
/// reported). A fresh Hub is not paired with its own portal, and every signed command — the
/// first Beacon or Node registration included — waits on that; the dashboard said nothing of
/// it (GitLab #109).
const AFTER_SETUP: &str = "/device?tab=configuration";

async fn setup_post(
    State(state): State<AppState>,
    addr: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<SetupForm>,
) -> Response {
    if !state.auth.setup_required().await {
        return Redirect::to("/login").into_response();
    }
    let ip = client_ip(addr, &headers);
    if let Err(err) = state.auth.setup_attempt(ip.as_deref()) {
        return retry_after(
            super::with_status(
                setup_form(&state, Some(err.slug())),
                StatusCode::TOO_MANY_REQUESTS,
            ),
            &err,
        );
    }
    if form.password != form.password_confirm {
        return setup_form(&state, Some("password_mismatch"));
    }

    let username = form.username.clone();
    let password = form.password.clone();
    let full_name = non_empty(form.full_name);

    match state
        .auth
        .create_superuser(form.username, form.password, full_name)
        .await
    {
        Ok(_user) => match state.auth.login(username, password, ip.clone()).await {
            Ok((token, _current)) => {
                let jar = CookieJar::new().add(auth::session_cookie(&token, state.cookie_secure));
                (jar, Redirect::to(AFTER_SETUP)).into_response()
            }
            Err(err) => {
                // The account exists at this point; a login-side failure
                // (e.g. a transient storage error) shouldn't strand the
                // operator without any signposting.
                tracing::error!(%err, "auto-login after setup failed");
                Redirect::to("/login").into_response()
            }
        },
        Err(err) => setup_form(&state, Some(err.slug())),
    }
}

async fn login_page(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(q): Query<LoginQuery>,
) -> Response {
    if session_user(&state, &jar).await.is_some() {
        return Redirect::to(safe_next(q.next.as_deref())).into_response();
    }
    render_page(
        &state,
        "pages/login.html",
        render::page_ctx(
            context! {
                error => Option::<String>::None,
                username => Option::<String>::None,
                next => q.next,
            },
            None,
        ),
    )
}

async fn login_post(
    State(state): State<AppState>,
    addr: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let ip = client_ip(addr, &headers);
    match state
        .auth
        .login(form.username.clone(), form.password, ip)
        .await
    {
        Ok((token, _current)) => {
            let jar = CookieJar::new().add(auth::session_cookie(&token, state.cookie_secure));
            (jar, Redirect::to(safe_next(form.next.as_deref()))).into_response()
        }
        Err(err) => {
            let status = if matches!(err, auth::AuthError::TooManyAttempts { .. }) {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::OK
            };
            let response = super::with_status(
                render_page(
                    &state,
                    "pages/login.html",
                    render::page_ctx(
                        context! { error => err.slug(), username => form.username, next => form.next },
                        None,
                    ),
                ),
                status,
            );
            retry_after(response, &err)
        }
    }
}

async fn logout(State(state): State<AppState>, jar: CookieJar) -> Response {
    if let Some(token) = jar.get(auth::SESSION_COOKIE).map(|c| c.value().to_string()) {
        let current_user = state.auth.validate(&token).await;
        state.auth.logout(&token, current_user.as_ref()).await;
    }
    let out_jar = CookieJar::new().add(auth::clear_session_cookie(state.cookie_secure));
    (out_jar, Redirect::to("/login")).into_response()
}

/// Resolves the session cookie in `jar` (if any) to a [`CurrentUser`] —
/// `Option<CurrentUser>` isn't usable as an axum extractor here since
/// `CurrentUser` only implements the plain (non-optional) `FromRequestParts`
/// (see `auth/extract.rs`), so login/logout resolve it by hand instead.
async fn session_user(state: &AppState, jar: &CookieJar) -> Option<CurrentUser> {
    let token = jar.get(auth::SESSION_COOKIE)?.value().to_string();
    state.auth.validate(&token).await
}

/// `pages/password.html`, with the top bar naming the page: it has no nav
/// entry, so without `page_title` the bar fell back to "ProXimos".
fn password_page_response(
    state: &AppState,
    current_user: &CurrentUser,
    ok: bool,
    error: Option<&str>,
) -> Response {
    render_page(
        state,
        "pages/password.html",
        render::page_ctx(
            context! {
                ok => ok,
                error => error,
                page_title => super::tr("common.nav.change_password"),
                page_icon => "key",
            },
            Some(current_user),
        ),
    )
}

async fn password_page(current_user: CurrentUser, State(state): State<AppState>) -> Response {
    password_page_response(&state, &current_user, false, None)
}

async fn password_post(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Form(form): Form<PasswordForm>,
) -> Response {
    if form.new != form.confirm {
        return password_page_response(&state, &current_user, false, Some("password_mismatch"));
    }

    match state
        .auth
        .change_password(current_user.id, &form.old, &form.new)
        .await
    {
        Ok(token) => {
            let jar = CookieJar::new().add(auth::session_cookie(&token, state.cookie_secure));
            let body = password_page_response(&state, &current_user, true, None);
            (jar, body).into_response()
        }
        Err(err) => {
            // LE-13: a wrong current password is an attempt on the account; it is audited as a
            // failed password change (a too-short new one is only a form slip, and is not).
            if matches!(err, auth::AuthError::InvalidCredentials) {
                state
                    .auth
                    .audit(
                        "password_change",
                        Some(&current_user),
                        None,
                        None,
                        Some(r#"{"refused":"invalid_credentials"}"#.to_string()),
                        false,
                    )
                    .await;
                // The login form's wording names a username this form does not have.
                return password_page_response(
                    &state,
                    &current_user,
                    false,
                    Some("wrong_current_password"),
                );
            }
            password_page_response(&state, &current_user, false, Some(err.slug()))
        }
    }
}

/// Smoke coverage for the templates this milestone rewired to translate
/// `error`/`ok` slugs instead of English messages — these routes had no
/// tests before M9, so a broken `t`/`replace` chain in the templates would
/// otherwise only surface as a 500 in production, not in `cargo test`.
#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use axum::body::to_bytes;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    use crate::auth::SESSION_COOKIE;
    use crate::routes::test_support::{login, test_state};

    fn peer() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345)
    }

    async fn body_text(response: axum::response::Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn login_post_wrong_password_renders_translated_error() {
        let (state, _dir) = test_state().await;
        let router = super::router().with_state(state);

        let mut request = Request::builder()
            .method("POST")
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("username=admin&password=wrong"))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(peer()));

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;
        // The slug itself must never leak into the page — it always
        // resolves to a translated sentence (English by default here).
        assert!(!body.contains("invalid_credentials"));
        assert!(body.contains("Invalid username or password"));
        // The healthcare tagline, back until the MDR owner reviews it
        // (status.md 2026-09-26).
        assert!(body.contains("Temperature Monitoring<br>"), "{body}");
        assert!(body.contains(">for Healthcare<"), "{body}");
    }

    /// UX-027: the open-redirect guard on `next`.
    #[test]
    fn safe_next_follows_only_local_paths() {
        use super::safe_next;
        assert_eq!(safe_next(Some("/alarms/x")), "/alarms/x");
        assert_eq!(safe_next(Some("/hubs/h?tab=config")), "/hubs/h?tab=config");
        for bad in [
            "//evil.example",
            "//evil.example/alarms/x",
            "https://evil.example",
            "/\\evil.example",
            "alarms/x",
            "",
            "/a\nb",
        ] {
            assert_eq!(safe_next(Some(bad)), "/", "{bad:?}");
        }
        assert_eq!(safe_next(None), "/");
    }

    /// UX-027: a successful login lands on `next` when it is local, on `/`
    /// otherwise, and the login page carries `next` into its form.
    #[tokio::test]
    async fn login_redirects_to_a_local_next_only() {
        let (state, _dir) = test_state().await;
        let router = super::router().with_state(state);

        let page = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/login?next=%2Falarms%2Fx")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_text(page).await;
        // minijinja's HTML escaping writes `/` as `&#x2f;`.
        assert!(
            body.contains(r#"<input type="hidden" name="next" value="&#x2f;alarms&#x2f;x">"#),
            "{body}"
        );

        for (next, want) in [
            ("%2Falarms%2Fx", "/alarms/x"),
            ("%2F%2Fevil.example", "/"),
            ("https%3A%2F%2Fevil.example%2F", "/"),
        ] {
            let mut request = Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "username=admin&password=correct-horse-battery&next={next}"
                )))
                .unwrap();
            request.extensions_mut().insert(ConnectInfo(peer()));
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::SEE_OTHER, "{next}");
            assert_eq!(
                response.headers().get(header::LOCATION).unwrap(),
                want,
                "{next}"
            );
        }
    }

    /// LE-11: past v1's ten failed logins a minute the form answers 429 with its own message.
    /// The first lockout lasts a minute (#103).
    #[tokio::test]
    async fn an_eleventh_failed_login_is_too_many_requests() {
        let (state, _dir) = test_state().await;
        let router = super::router().with_state(state);
        let attempt = || {
            let mut request = Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("username=admin&password=wrong"))
                .unwrap();
            request.extensions_mut().insert(ConnectInfo(peer()));
            router.clone().oneshot(request)
        };
        for _ in 0..10 {
            assert_eq!(attempt().await.unwrap().status(), StatusCode::OK);
        }
        let response = attempt().await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .map(|v| v.as_bytes()),
            Some(&b"60"[..])
        );
        assert!(body_text(response).await.contains("Too many attempts"));

        // #103: a refusal during the lockout says what is left of it.
        let response = attempt().await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let secs: u64 = response.headers()[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((1..=60).contains(&secs), "{secs}");
    }

    #[tokio::test]
    async fn setup_post_password_mismatch_renders_translated_error() {
        // A fresh AuthService with no superuser yet, unlike `test_state()`
        // (which always seeds one) — `setup_post` only reaches the
        // mismatch check while `setup_required()` still holds.
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap().to_string();
        let (state, _dir2) = test_state().await;
        let mut state = state;
        state.auth =
            crate::auth::AuthService::open(&data_dir, None, crate::durable::test_gate(&data_dir))
                .unwrap();
        let router = super::router().with_state(state);

        let mut request = Request::builder()
            .method("POST")
            .uri("/setup")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "username=newadmin&password=abcdefgh&password_confirm=different",
            ))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(peer()));

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;
        assert!(!body.contains("password_mismatch"));
        assert!(body.contains("Passwords do not match"));
    }

    /// A router over a fresh AuthService with no superuser yet (so `/setup` is open).
    async fn setup_router() -> (
        axum::Router,
        crate::auth::AuthService,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap().to_string();
        let (mut state, dir2) = test_state().await;
        state.auth =
            crate::auth::AuthService::open(&data_dir, None, crate::durable::test_gate(&data_dir))
                .unwrap();
        state.data_dir = data_dir;
        let auth = state.auth.clone();
        (super::router().with_state(state), auth, dir, dir2)
    }

    fn setup_request(body: &str) -> Request<Body> {
        let mut request = Request::builder()
            .method("POST")
            .uri("/setup")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body.to_string()))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(peer()));
        request
    }

    const ACCOUNT: &str = "username=newadmin&password=abcdefgh&password_confirm=abcdefgh";

    /// GitLab #154: the radio mode is chosen at provisioning, `/setup` does not offer it.
    #[tokio::test]
    async fn setup_page_does_not_offer_a_transport() {
        let (router, _auth, _d, _d2) = setup_router().await;
        let request = Request::builder()
            .uri("/setup")
            .body(Body::empty())
            .unwrap();
        let body = body_text(router.oneshot(request).await.unwrap()).await;
        assert!(body.contains(r#"name="password_confirm""#), "{body}");
        assert!(!body.contains("Communication type"), "{body}");
        assert!(!body.contains(r#"name="radio_mode""#));
    }

    #[tokio::test]
    async fn setup_redirects_to_pairing_and_writes_no_transport_request() {
        let (router, auth, dir, _d2) = setup_router().await;
        let response = router.oneshot(setup_request(ACCOUNT)).await.unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(
            response.headers().get(header::SET_COOKIE).is_some(),
            "logged in"
        );
        // GitLab #109: setup ends on the pairing card.
        assert_eq!(
            response.headers()[header::LOCATION],
            "/device?tab=configuration"
        );
        assert!(!auth.setup_required().await);
        // GitLab #154: the Hub keeps the mode chosen at provisioning.
        assert!(!dir.path().join("requested-radio-mode").exists());
    }

    #[tokio::test]
    async fn setup_ignores_a_stale_radio_mode_field() {
        let (router, auth, dir, _d2) = setup_router().await;
        let response = router
            .oneshot(setup_request(&format!("{ACCOUNT}&radio_mode=lorawan")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(!auth.setup_required().await);
        assert!(!dir.path().join("requested-radio-mode").exists());
    }

    #[tokio::test]
    async fn password_post_mismatch_renders_translated_error() {
        let (state, _dir) = test_state().await;
        let token = login(&state, "admin", "correct-horse-battery").await;
        let router = super::router().with_state(state);

        let request = Request::builder()
            .method("POST")
            .uri("/settings/password")
            .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "old=correct-horse-battery&new=short1&confirm=short2",
            ))
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;
        assert!(!body.contains("password_mismatch"));
        assert!(body.contains("do not match"));
    }

    /// UX-052: a wrong current password says so, not the login form's
    /// "invalid username or password", and the top bar names the page.
    #[tokio::test]
    async fn password_post_wrong_current_password_says_so() {
        let (state, _dir) = test_state().await;
        let token = login(&state, "admin", "correct-horse-battery").await;
        let router = super::router().with_state(state);

        let request = Request::builder()
            .method("POST")
            .uri("/settings/password")
            .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "old=not-my-password&new=another-pass-1&confirm=another-pass-1",
            ))
            .unwrap();

        let body = body_text(router.oneshot(request).await.unwrap()).await;
        assert!(body.contains("The current password is wrong."), "{body}");
        assert!(!body.contains("Invalid username or password"));
        let topbar = body
            .split("<header")
            .nth(1)
            .and_then(|s| s.split("</header>").next())
            .expect("the top bar");
        assert!(topbar.contains("Change password"), "{topbar}");
    }
}

/// LE-12: behind nginx (a loopback peer) the audit takes X-Real-IP; from anywhere else the
/// header is ignored.
#[test]
fn the_audit_ip_is_the_client_behind_nginx_only() {
    let mut headers = HeaderMap::new();
    headers.insert("x-real-ip", "192.168.0.50".parse().expect("header"));
    let local: SocketAddr = "127.0.0.1:5000".parse().expect("addr");
    let remote: SocketAddr = "10.0.0.9:5000".parse().expect("addr");
    assert_eq!(
        client_ip(ConnectInfo(local), &headers).as_deref(),
        Some("192.168.0.50")
    );
    assert_eq!(
        client_ip(ConnectInfo(remote), &headers).as_deref(),
        Some("10.0.0.9")
    );
    assert_eq!(
        client_ip(ConnectInfo(local), &HeaderMap::new()).as_deref(),
        Some("127.0.0.1")
    );
}
