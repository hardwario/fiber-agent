//! The client address of the request being served, for the audit log (C36-11, smoke test of
//! combined36): only the login rows carried an IP, because only the login route read one.
//! A middleware resolves it once per request and scopes it, the way `i18n::lang_middleware`
//! scopes the language, and [`super::AuthService::audit`] falls back to it.

use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;

use axum::extract::{ConnectInfo, Request};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;

tokio::task_local! {
    static CLIENT_IP: Option<String>;
}

/// An IP address or a CIDR range from `trusted_proxies`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpRange {
    base: IpAddr,
    prefix: u8,
}

impl IpRange {
    /// `192.0.2.7`, `172.16.0.0/12`, `fd00::/8`; `None` for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        let (addr, prefix) = match text.trim().split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix.parse::<u8>().ok()?)),
            None => (text.trim(), None),
        };
        let base: IpAddr = addr.parse().ok()?;
        let max = if base.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        (prefix <= max).then_some(Self { base, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let bits = |ip: IpAddr| -> u128 {
            match ip {
                IpAddr::V4(v4) => u128::from(u32::from(v4)),
                IpAddr::V6(v6) => u128::from(v6),
            }
        };
        let width: u32 = if self.base.is_ipv4() { 32 } else { 128 };
        if ip.is_ipv4() != self.base.is_ipv4() {
            return false;
        }
        if self.prefix == 0 {
            return true;
        }
        let shift = width - u32::from(self.prefix);
        bits(ip) >> shift == bits(self.base) >> shift
    }
}

static TRUSTED: OnceLock<Vec<IpRange>> = OnceLock::new();

/// Sets the configured `trusted_proxies` once at startup (`http::app`); entries that do not
/// parse were refused by the config validation already.
pub fn set_trusted_proxies(entries: &[String]) {
    let _ = TRUSTED.set(entries.iter().filter_map(|e| IpRange::parse(e)).collect());
}

/// The TCP peer is nginx on a Hub, so every row said 127.0.0.1 (LE-12, local E2E
/// 2026-09-26): behind a loopback peer, or a configured trusted proxy, the `X-Real-IP` it sets
/// is the client. From any other peer the header is not trusted, since the client could set
/// it.
pub fn client_ip(addr: SocketAddr, headers: &HeaderMap) -> String {
    client_ip_with(addr, headers, TRUSTED.get().map_or(&[], Vec::as_slice))
}

fn client_ip_with(addr: SocketAddr, headers: &HeaderMap, trusted: &[IpRange]) -> String {
    let peer = addr.ip();
    if peer.is_loopback() || trusted.iter().any(|range| range.contains(peer)) {
        if let Some(real) = headers
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
        {
            return real.to_string();
        }
    }
    peer.to_string()
}

pub async fn client_ip_middleware(request: Request, next: Next) -> Response {
    let ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| client_ip(*addr, request.headers()));
    CLIENT_IP.scope(ip, next.run(request)).await
}

/// The address of the request this task is serving; `None` outside a request (background
/// services) or when the peer was not recorded.
pub fn current_client_ip() -> Option<String> {
    CLIENT_IP.try_with(Clone::clone).ok().flatten()
}

/// Runs `fut` with `ip` as its request's client: for work a route spawns off its request, since a
/// spawned future does not inherit task-locals (S38-1: the draft's send walk wrote its audit
/// rows without an address).
pub async fn scope_client_ip<F: std::future::Future>(ip: Option<String>, fut: F) -> F::Output {
    CLIENT_IP.scope(ip, fut).await
}

#[cfg(test)]
pub(crate) async fn with_client_ip<F: std::future::Future>(ip: &str, fut: F) -> F::Output {
    scope_client_ip(Some(ip.to_string()), fut).await
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn a_trusted_proxy_names_the_client_and_others_do_not() {
        let caddy = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 18, 0, 3)), 50000);
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "198.51.100.4".parse().unwrap());
        let trusted = [IpRange::parse("172.16.0.0/12").unwrap()];
        assert_eq!(client_ip_with(caddy, &headers, &trusted), "198.51.100.4");
        assert_eq!(client_ip_with(caddy, &headers, &[]), "172.18.0.3");
        let other = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 50000);
        assert_eq!(client_ip_with(other, &headers, &trusted), "192.0.2.1");

        assert_eq!(
            IpRange::parse("fd00::/8").map(|r| r.contains("fd12::1".parse().unwrap())),
            Some(true)
        );
        assert!(IpRange::parse("192.0.2.7")
            .unwrap()
            .contains("192.0.2.7".parse().unwrap()));
        assert!(!IpRange::parse("192.0.2.7")
            .unwrap()
            .contains("192.0.2.8".parse().unwrap()));
        assert!(IpRange::parse("10.0.0.0/33").is_none());
        assert!(IpRange::parse("caddy").is_none());
    }

    #[tokio::test]
    async fn the_middleware_scopes_the_client_behind_nginx() {
        let app = Router::new()
            .route(
                "/",
                get(|| async { current_client_ip().unwrap_or_default() }),
            )
            .layer(axum::middleware::from_fn(client_ip_middleware));
        let mut request = Request::builder()
            .uri("/")
            .header("x-real-ip", "192.0.2.77")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            40000,
        )));
        let response = app.oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"192.0.2.77");
        assert_eq!(current_client_ip(), None, "nothing outside a request");
    }
}
