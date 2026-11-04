//! CSRF defence for the cookie-session model (plan/portal/design.md §4:
//! "SameSite + Origin check middleware on unsafe methods", amended for
//! GitLab #105). SameSite=Lax already keeps the session cookie off
//! cross-site form posts in current browsers. This middleware does not
//! depend on the cookie at all, which is what covers `/login` (a login CSRF
//! needs no session to ride on) and the cases where SameSite alone isn't
//! trusted (older browsers, subdomain takeover).
//!
//! An unsafe request is judged by the first of these headers it carries,
//! and by that one alone:
//!
//! 1. **`Origin`**: its authority must equal `Host`. Current browsers send
//!    it on every POST, as `null` when a referrer policy hides the origin,
//!    and `null` is refused.
//! 2. **`Sec-Fetch-Site`**, when `Origin` is missing (stripped by a proxy or
//!    an extension, or a browser that leaves it off a form post):
//!    `same-origin` passes, and so does `none`, a request the user started
//!    from the address bar or a bookmark. `same-site`, `cross-site` and
//!    anything else are refused. Page script cannot set a `Sec-` header, so
//!    a cross-site page cannot forge `same-origin`.
//! 3. **`Referer`**, when both are missing: its authority must equal `Host`,
//!    and a malformed one is refused. Only old browsers get this far, and
//!    the attacking page can tell them to drop the header, so this refuses
//!    what it can see and nothing more.
//! 4. **None of the three**: the request passes. That is a client that is
//!    not a browser: curl, the tests, the desktop client
//!    (plan/desktop/README.md). Or it is a browser too old to send any of
//!    them, for which SameSite=Lax is the remaining defence on authenticated
//!    routes and nothing guards `/login`. Refusing here would lock every
//!    non-browser client out to protect a browser nobody should be running.
//!
//! `Origin` and `Referer` are checked the same way: their authority against
//! the `Host` header nginx forwards, case-insensitively and port included.

use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Fetch Metadata request header. `axum::http::header` has no constant
/// for it.
const SEC_FETCH_SITE: &str = "sec-fetch-site";

fn is_unsafe_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// Extracts the `host[:port]` authority from an absolute URL
/// (`scheme://host[:port][/path][?query][#fragment]`): an `Origin` value,
/// which ends at the authority, or a `Referer`, which goes on. `None` if
/// `url` doesn't look like `scheme://authority`.
fn url_authority(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    Some(rest.split(['/', '?', '#']).next().unwrap_or(rest))
}

/// `true` if `url`'s authority matches the `Host` header value
/// case-insensitively (including port — `example.com:8080` does not match
/// `example.com`).
fn authority_matches_host(url: &str, host: &str) -> bool {
    match url_authority(url) {
        Some(authority) => authority.eq_ignore_ascii_case(host),
        None => false,
    }
}

/// Whether an unsafe request carrying `headers` may reach its handler. The
/// order and the reasons are in the module docs. A header that is present
/// but not readable as text counts as present and failing, never as absent.
fn may_proceed(headers: &HeaderMap) -> bool {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let matches_host = |url: &HeaderValue| match (url.to_str(), host) {
        (Ok(url), Some(host)) => authority_matches_host(url, host),
        _ => false,
    };

    if let Some(origin) = headers.get(header::ORIGIN) {
        return matches_host(origin);
    }
    if let Some(site) = headers.get(SEC_FETCH_SITE) {
        return matches!(site.as_bytes(), b"same-origin" | b"none");
    }
    if let Some(referer) = headers.get(header::REFERER) {
        return matches_host(referer);
    }
    true
}

/// Rejects cross-origin `POST`/`PUT`/`PATCH`/`DELETE` requests with 403
/// `cross-origin request blocked`, judged by `Origin`, then
/// `Sec-Fetch-Site`, then `Referer` (module docs). A request that carries
/// none of the three passes untouched: non-browser clients (curl, tests,
/// the desktop client) never send them.
pub async fn origin_guard(request: Request, next: Next) -> Response {
    if is_unsafe_method(request.method()) && !may_proceed(request.headers()) {
        return (StatusCode::FORBIDDEN, "cross-origin request blocked").into_response();
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use axum::http::{header, HeaderMap, HeaderName, HeaderValue};

    use super::{authority_matches_host, may_proceed, SEC_FETCH_SITE};

    #[test]
    fn matching_host_and_port_passes() {
        assert!(authority_matches_host(
            "https://example.com:8080",
            "example.com:8080"
        ));
    }

    #[test]
    fn matching_host_default_port_omitted_both_sides() {
        assert!(authority_matches_host("https://example.com", "example.com"));
    }

    #[test]
    fn different_port_fails() {
        assert!(!authority_matches_host(
            "https://example.com:8080",
            "example.com:9090"
        ));
    }

    #[test]
    fn origin_with_port_vs_host_without_fails() {
        assert!(!authority_matches_host(
            "https://example.com:8080",
            "example.com"
        ));
    }

    #[test]
    fn different_host_fails() {
        assert!(!authority_matches_host(
            "https://evil.example",
            "example.com"
        ));
    }

    #[test]
    fn case_insensitive_match() {
        assert!(authority_matches_host("https://Example.COM", "example.com"));
    }

    #[test]
    fn malformed_origin_fails() {
        assert!(!authority_matches_host("not-a-url", "example.com"));
    }

    #[test]
    fn scheme_is_ignored() {
        assert!(authority_matches_host(
            "http://localhost:3000",
            "localhost:3000"
        ));
    }

    /// A `Referer` goes on past the authority; path, query and fragment
    /// must not leak into the comparison.
    #[test]
    fn referer_path_query_and_fragment_are_ignored() {
        for url in [
            "https://example.com/settings/users?tab=1",
            "https://example.com?next=/",
            "https://example.com#top",
        ] {
            assert!(authority_matches_host(url, "example.com"), "{url}");
        }
    }

    const HOST: &str = "proximos-a1b2c3d4.local";
    const SAME: &str = "https://proximos-a1b2c3d4.local";
    const FOREIGN: &str = "https://evil.example";

    /// `Host` plus `pairs`, as a browser would send them to the Hub.
    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(header::HOST, HeaderValue::from_static(HOST));
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn same_origin_passes_and_foreign_origin_is_refused() {
        assert!(may_proceed(&headers(&[("origin", SAME)])));
        assert!(!may_proceed(&headers(&[("origin", FOREIGN)])));
        assert!(!may_proceed(&headers(&[("origin", "null")])));
    }

    /// `Origin`, when present, decides alone: the weaker headers behind it
    /// cannot launder a foreign or `null` one.
    #[test]
    fn origin_outranks_the_fallbacks() {
        assert!(!may_proceed(&headers(&[
            ("origin", FOREIGN),
            (SEC_FETCH_SITE, "same-origin"),
        ])));
        assert!(!may_proceed(&headers(&[
            ("origin", "null"),
            ("referer", "https://proximos-a1b2c3d4.local/login"),
        ])));
    }

    #[test]
    fn without_origin_sec_fetch_site_decides() {
        for (site, allowed) in [
            ("same-origin", true),
            ("none", true),
            ("same-site", false),
            ("cross-site", false),
            // Not a value the Fetch Metadata spec defines: refused, not
            // waved through to the Referer check.
            ("bogus", false),
        ] {
            assert_eq!(
                may_proceed(&headers(&[(SEC_FETCH_SITE, site)])),
                allowed,
                "Sec-Fetch-Site: {site}"
            );
        }
    }

    /// `Sec-Fetch-Site` outranks `Referer`: a same-origin request is not
    /// refused over a Referer, and a cross-site one is not let through by one.
    #[test]
    fn sec_fetch_site_outranks_referer() {
        assert!(!may_proceed(&headers(&[
            (SEC_FETCH_SITE, "cross-site"),
            ("referer", "https://proximos-a1b2c3d4.local/"),
        ])));
    }

    #[test]
    fn without_origin_or_sec_fetch_site_referer_decides() {
        assert!(may_proceed(&headers(&[(
            "referer",
            "https://proximos-a1b2c3d4.local/settings/users?tab=1"
        )])));
        assert!(!may_proceed(&headers(&[(
            "referer",
            "https://evil.example/proximos-a1b2c3d4.local"
        )])));
        assert!(!may_proceed(&headers(&[(
            "referer",
            "https://proximos-a1b2c3d4.local:8443/"
        )])));
        // Malformed: no scheme, or a relative reference.
        assert!(!may_proceed(&headers(&[(
            "referer",
            "proximos-a1b2c3d4.local/"
        )])));
        assert!(!may_proceed(&headers(&[("referer", "/settings")])));
    }

    /// curl, the tests and the desktop client send none of the three.
    #[test]
    fn a_request_with_none_of_the_three_passes() {
        assert!(may_proceed(&headers(&[])));
        // Without `Host` too: nothing claims the request came from a page.
        assert!(may_proceed(&HeaderMap::new()));
    }

    #[test]
    fn a_url_header_without_host_is_refused() {
        let mut map = HeaderMap::new();
        map.insert(header::REFERER, HeaderValue::from_static(SAME));
        assert!(!may_proceed(&map));
        let mut map = HeaderMap::new();
        map.insert(header::ORIGIN, HeaderValue::from_static(SAME));
        assert!(!may_proceed(&map));
    }

    /// An `Origin` that is not readable as text used to count as absent, and
    /// so passed. It is present, so it is judged, and it fails.
    #[test]
    fn an_unreadable_origin_is_refused_not_ignored() {
        let mut map = headers(&[]);
        map.insert(
            header::ORIGIN,
            HeaderValue::from_bytes(b"https://proximos-a1b2c3d4.local\xff").unwrap(),
        );
        assert!(!may_proceed(&map));
    }
}
