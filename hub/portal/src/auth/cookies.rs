//! Session cookie construction. `secure` is a plain bool rather than a
//! config field — the routes work package reads `PortalConfig` and decides
//! (e.g. `bind_addr` starts with `127.0.0.1` / a `cookie_secure` override);
//! this crate boundary is not touched here (see M2-B scope notes).

use axum_extra::extract::cookie::{Cookie, SameSite};
use cookie::time::Duration;

use crate::auth::extract::SESSION_COOKIE;

/// How long the browser keeps the cookie: the server's absolute session lifetime, longer than
/// its idle timeout on purpose (LE-19, local E2E 2026-09-26): the server slides a session while
/// it is used, but the cookie was never re-issued, so a browser dropped it 24 h after login in
/// the middle of work. The server ends a session 7 days after login at the latest (R-11).
const COOKIE_MAX_AGE_SECS: i64 = crate::auth::service::ABSOLUTE_TTL_SECS;

/// Builds the `Set-Cookie` value for a freshly issued session token.
pub fn session_cookie(token: &str, secure: bool) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, token.to_string()))
        .http_only(true)
        .same_site(SameSite::Lax)
        .path("/")
        .secure(secure)
        .max_age(Duration::seconds(COOKIE_MAX_AGE_SECS))
        .build()
}

/// Builds a `Set-Cookie` value that expires the session cookie immediately
/// (logout).
pub fn clear_session_cookie(secure: bool) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, ""))
        .http_only(true)
        .same_site(SameSite::Lax)
        .path("/")
        .secure(secure)
        .max_age(Duration::ZERO)
        .build()
}
