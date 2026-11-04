//! Auth core (M2-B): [`AuthService`] (sessions, password hashing, admin
//! user API, audit), the `CurrentUser`/`RequireManager`/`RequireAdmin`
//! extractors, the origin-check CSRF guard, and session cookie helpers.
//!
//! Nothing here is wired into `http.rs` yet — that's the parallel M2-C
//! routes work package, wiring against this module's public API as a
//! binding contract. Until then most of this is exercised only by this
//! module's own tests, which trips `dead_code` on an otherwise-intentional
//! public surface; suppressed here rather than left for M2-C to clean up
//! (same pattern as `src/state/mod.rs`, `src/mqtt/mod.rs`).
#![allow(dead_code, unused_imports)]

mod client_ip;
mod cookies;
mod extract;
mod guard;
mod service;
mod throttle;
mod types;

#[cfg(test)]
pub(crate) use client_ip::with_client_ip;
pub use client_ip::{
    client_ip, client_ip_middleware, current_client_ip, scope_client_ip, set_trusted_proxies,
    IpRange,
};
pub use cookies::{clear_session_cookie, session_cookie};
pub use extract::{
    AuthRedirect, ForbiddenPage, RequireAdmin, RequireAdminRejection, RequireManager,
    RoleRejection, SESSION_COOKIE,
};
pub use guard::origin_guard;
pub(crate) use service::{generate_token, sha256_hex};
pub use service::{AuthService, NewUserInput};
pub use types::{AuthError, CurrentUser, Role};
