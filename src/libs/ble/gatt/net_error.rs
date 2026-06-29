//! Shared error categorization for network provisioning.
//!
//! Maps an nmcli (or system) error string to a small, stable set of
//! categories so the WiFi (FB02–FB08) and LAN (FB09/FB0C) paths can report
//! failures consistently. This is the categorization primitive behind the
//! structured-error work tracked in issue #9; the LAN path uses it now, the
//! WiFi `failure()` refactor to adopt it is a follow-up.

use serde::{Deserialize, Serialize};

/// Coarse classification of a provisioning failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkErrorCategory {
    /// Malformed/invalid IP address, prefix, gateway or DNS value.
    InvalidIp,
    /// Gateway is not in the configured subnet / not reachable.
    GatewayUnreachable,
    /// Authentication failed (e.g. wrong WiFi password / missing secrets).
    AuthFailed,
    /// Target network/connection not found.
    NotFound,
    /// Operation timed out.
    Timeout,
    /// Anything not matched above (carries the raw message via the status `error`).
    Other,
}

impl NetworkErrorCategory {
    /// Stable machine-readable tag, suitable as a prefix in a status `error` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidIp => "invalid_ip",
            Self::GatewayUnreachable => "gateway_unreachable",
            Self::AuthFailed => "auth_failed",
            Self::NotFound => "not_found",
            Self::Timeout => "timeout",
            Self::Other => "other",
        }
    }
}

/// Best-effort classification of an nmcli/system error string.
///
/// Matching is substring-based and intentionally conservative: unknown
/// messages fall through to [`NetworkErrorCategory::Other`] so the raw text
/// is still surfaced to the caller.
pub fn categorize(stderr: &str) -> NetworkErrorCategory {
    let s = stderr.to_lowercase();
    if s.contains("gateway") || s.contains("unreachable") || s.contains("not reachable") {
        NetworkErrorCategory::GatewayUnreachable
    } else if s.contains("invalid")
        && (s.contains("address") || s.contains("ip") || s.contains("prefix"))
    {
        NetworkErrorCategory::InvalidIp
    } else if s.contains("secrets were required")
        || s.contains("key-mgmt")
        || s.contains("802-11-wireless-security")
        || s.contains("authentication")
    {
        NetworkErrorCategory::AuthFailed
    } else if s.contains("no network with ssid")
        || s.contains("unknown connection")
        || s.contains("not found")
    {
        NetworkErrorCategory::NotFound
    } else if s.contains("timeout") || s.contains("timed out") {
        NetworkErrorCategory::Timeout
    } else {
        NetworkErrorCategory::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categorize_gateway() {
        assert_eq!(
            categorize("Error: ipv4.gateway: gateway is unreachable"),
            NetworkErrorCategory::GatewayUnreachable
        );
    }

    #[test]
    fn categorize_invalid_ip() {
        assert_eq!(
            categorize("Error: invalid IP address '999.1.1.1'"),
            NetworkErrorCategory::InvalidIp
        );
    }

    #[test]
    fn categorize_auth() {
        assert_eq!(
            categorize("Error: 802-11-wireless-security.key-mgmt: property is missing"),
            NetworkErrorCategory::AuthFailed
        );
    }

    #[test]
    fn categorize_not_found() {
        assert_eq!(
            categorize("Error: Unknown connection 'eth0'."),
            NetworkErrorCategory::NotFound
        );
    }

    #[test]
    fn categorize_timeout() {
        assert_eq!(
            categorize("Error: Connection activation timed out"),
            NetworkErrorCategory::Timeout
        );
    }

    #[test]
    fn categorize_unknown_is_other() {
        assert_eq!(categorize("some weird message"), NetworkErrorCategory::Other);
    }

    #[test]
    fn as_str_is_stable() {
        assert_eq!(NetworkErrorCategory::GatewayUnreachable.as_str(), "gateway_unreachable");
        assert_eq!(NetworkErrorCategory::InvalidIp.as_str(), "invalid_ip");
    }
}
