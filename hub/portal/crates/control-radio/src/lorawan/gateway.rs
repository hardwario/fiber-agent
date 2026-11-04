//! Gateway EUI discovery and the `gateway-*` commands — `lorawan.md` §6, §7.
//!
//! **Discovery is a retained-topic read, and that is not merely the nicer option.**
//! `chirpstack-mqtt-forwarder` publishes a retained `{prefix}gateway/{gateway_id}/state/conn`
//! on local mosquitto, and the EUI is the `{gateway_id}` **path segment** — one read, no
//! protobuf decode, available any time after the forwarder's first connect.
//!
//! v1 instead ran a three-source shell ladder — `journalctl -u chirpstack-concentratord`
//! grepped for `gateway_id: "<hex>"`, then `journalctl -u chirpstack`, then a live
//! `mosquitto_sub` — 15 times at 2 s intervals, with its own comment conceding "log format
//! isn't a stable contract". Decision #6 rules that out independently: no shell-outs, no
//! `journalctl`. So the retained read is the only compliant mechanism, not a preference.
//!
//! The `{prefix}` is **configuration, not a constant**: v1 sets `topic_prefix = "eu868"` in
//! the forwarder's TOML, and upstream renders an empty prefix as no prefix at all. It must
//! come from the same renderer input the forwarder config is generated from.

use std::time::Duration;

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::client::ChirpStack;
use crate::lorawan::eui::Eui;
use crate::lorawan::model::{Gateway, GatewaySpec};

/// How long `create` polls the retained topic before concluding there is no radio.
///
/// **The plan does not give a number** — §6 says only "up to a deadline". Chosen here to
/// clear v1's own ladder (15 tries at 2 s = 30 s) with margin, since the forwarder only has
/// to have connected once and the message is retained, so this is a first-boot race rather
/// than a poll loop. Carried to the plan-feedback set for confirmation.
pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(45);

/// A gateway counts as online when last contact was within this window.
///
/// v1's `GATEWAY_ONLINE_WINDOW_SECS`, kept as a named constant in `control_radio` rather than
/// a config value (§6).
pub const GATEWAY_ONLINE_WINDOW: Duration = Duration::from_secs(300);

/// Reads retained topics. A trait so discovery is testable without a broker — the same reason
/// the ChirpStack client is one.
#[async_trait::async_trait]
pub trait RetainedTopics: Send + Sync {
    /// Every retained topic matching `filter`, as topic names. Payloads are deliberately not
    /// returned: the EUI is in the path, and reading the payload would mean decoding
    /// protobuf for a fact the topic name already carries.
    async fn retained_topic_names(&self, filter: &str, timeout: Duration) -> Result<Vec<String>>;
}

/// Build the subscribe filter for gateway state topics.
///
/// An empty prefix renders as no prefix at all, matching upstream — so
/// `prefix_of("") == "gateway/+/state/conn"`, not `"/gateway/..."`.
pub fn state_topic_filter(prefix: &str) -> String {
    let p = prefix.trim_end_matches('/');
    if p.is_empty() {
        "gateway/+/state/conn".to_string()
    } else {
        format!("{p}/gateway/+/state/conn")
    }
}

/// Pull the `{gateway_id}` segment out of a state topic.
///
/// Returns `None` for a topic that does not match the shape, rather than guessing — a
/// neighbouring retained topic must never be mistaken for a gateway.
pub fn eui_from_state_topic(topic: &str) -> Option<Result<Eui>> {
    let parts: Vec<&str> = topic.split('/').collect();
    // ... gateway / {id} / state / conn
    let n = parts.len();
    if n < 4 {
        return None;
    }
    if parts[n - 1] != "conn" || parts[n - 2] != "state" || parts[n - 4] != "gateway" {
        return None;
    }
    Some(Eui::parse(parts[n - 3]))
}

/// Discover the local concentrator's EUI, or `None` when no radio answered.
///
/// `None` is a degraded success, not a failure: the qemuarm64 smoke image has no RAK5146, the
/// forwarder never publishes, and the boot must still go green.
pub async fn discover(
    topics: &dyn RetainedTopics,
    prefix: &str,
    timeout: Duration,
) -> Result<Option<Eui>> {
    discover_preferring(topics, prefix, timeout, &[]).await
}

/// Every EUI with a retained state topic under `prefix`, sorted and deduplicated.
pub async fn discover_all(
    topics: &dyn RetainedTopics,
    prefix: &str,
    timeout: Duration,
) -> Result<Vec<Eui>> {
    let filter = state_topic_filter(prefix);
    let names = topics.retained_topic_names(&filter, timeout).await?;
    let mut found: Vec<Eui> = Vec::new();
    for name in &names {
        if let Some(parsed) = eui_from_state_topic(name) {
            found.push(parsed?);
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

/// [`discover`], but when several gateways have retained state, the one among `known`
/// (registered in ChirpStack as this Hub's own gateway) wins.
///
/// A retained `state/conn` outlives its gateway, and a cluster Client Hub's forwarder publishes
/// on this broker too: a swapped card or a Client leaves a second document here for good. Without
/// a tie-breaker every later `radio-setup` refuses. The tie-breaker is only ever applied when
/// exactly one of the candidates is known; zero or several known still refuse, because picking
/// the wrong one silently drops every uplink from the other.
pub async fn discover_preferring(
    topics: &dyn RetainedTopics,
    prefix: &str,
    timeout: Duration,
    known: &[Eui],
) -> Result<Option<Eui>> {
    let found = discover_all(topics, prefix, timeout).await?;
    pick(found, known, &state_topic_filter(prefix))
}

/// The selection rule of [`discover_preferring`], separate so it is testable on its own.
pub fn pick(found: Vec<Eui>, known: &[Eui], filter: &str) -> Result<Option<Eui>> {
    if found.len() <= 1 {
        return Ok(found.into_iter().next());
    }
    let preferred: Vec<&Eui> = found.iter().filter(|e| known.contains(e)).collect();
    if let [one] = preferred.as_slice() {
        let stale: Vec<&str> = found.iter().filter(|e| e != one).map(Eui::as_str).collect();
        eprintln!(
            "radio: several gateways have retained state; using the local {} and ignoring {}",
            one.as_str(),
            stale.join(", ")
        );
        return Ok(Some((*one).clone()));
    }
    // More than one local forwarder is a misconfiguration, not something to pick from:
    // registering the wrong one silently drops every uplink from the other.
    Err(Error::new(
        Kind::InvalidArgument,
        Transport::Lorawan,
        Component::MqttBroker,
        format!(
            "{} gateways are publishing retained state under {filter:?} ({}); \
             exactly one local concentrator is expected — pass --gateway-eui to choose",
            found.len(),
            found.iter().map(Eui::as_str).collect::<Vec<_>>().join(", ")
        ),
    ))
}

/// One row of `radio/gateways` and of `gateway-list --json`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct GatewayStatus {
    pub gateway_eui: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rssi: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snr: Option<f64>,
    pub radio: &'static str,
}

/// Whether the management API itself is reachable and accepted our credential.
///
/// Top-level and separate from the gateway list, because conflating them is the v1 defect
/// issue #8 names: `get_gateways_status` returned a gateway list, so a rejected login reported
/// **every gateway offline** — operationally indistinguishable from a site-wide radio outage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiStatus {
    Ok,
    AuthFailed,
    Unreachable,
}

impl ApiStatus {
    /// Classify a failed poll. An auth failure and an unreachable service are different
    /// operator actions, so they are different values.
    pub fn from_error(e: &Error) -> Self {
        match e.kind() {
            Kind::Unauthenticated | Kind::PermissionDenied | Kind::NotBootstrapped => {
                ApiStatus::AuthFailed
            }
            _ => ApiStatus::Unreachable,
        }
    }
}

/// The `radio/gateways` payload (§6).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct GatewaysReport {
    pub timestamp: String,
    pub api_status: ApiStatus,
    /// Always the last known state from the local channel — **never erased or replaced by an
    /// API auth failure**. An operator who cannot authenticate still needs to see which
    /// gateways were last heard from.
    pub gateways: Vec<GatewayStatus>,
}

/// Decide online/offline from `last_seen_at`.
///
/// Never-seen is offline. A `last_seen` in the future — clock skew, which happens on a device
/// that boots before NTP — counts as just-seen rather than wrapping into "very old".
pub fn is_online(last_seen_at: Option<&str>, now_unix: i64) -> bool {
    let Some(ts) = last_seen_at else {
        return false;
    };
    let Some(seen) = parse_rfc3339_unix(ts) else {
        return false;
    };
    if seen >= now_unix {
        return true;
    }
    (now_unix - seen) as u64 <= GATEWAY_ONLINE_WINDOW.as_secs()
}

/// Wall-clock seconds since the epoch, for the online window and the report timestamp.
///
/// A clock before the epoch is not representable in this direction and means the RTC has not
/// been set yet; `0` then makes every gateway read as just-seen rather than crashing the
/// health poll, which matches [`is_online`]'s deliberate clock-skew tolerance.
pub fn now_unix() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// Unix seconds → RFC3339 UTC, the spelling §6 gives `radio/gateways`' `timestamp`.
///
/// The inverse of [`parse_rfc3339_unix`], and here for the same reason: one field needs it, and
/// a date-time dependency for one field is not worth the standalone guarantee it spends.
pub fn format_rfc3339(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    // Civil-from-days, Howard Hinnant's algorithm — the exact inverse of the days-from-civil
    // used below, so the two round-trip by construction rather than by coincidence.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Minimal RFC3339 → unix seconds. The crate deliberately carries no date-time dependency;
/// only this one comparison needs it, and ChirpStack's timestamps are always UTC `Z`.
pub(crate) fn parse_rfc3339_unix(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from civil epoch (Howard Hinnant's algorithm).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + sec)
}

pub fn to_status(g: &Gateway, now_unix: i64) -> GatewayStatus {
    GatewayStatus {
        gateway_eui: g.gateway_id.as_str().to_string(),
        name: g.name.clone(),
        hostname: None,
        ip: None,
        status: if is_online(g.last_seen_at.as_deref(), now_unix) {
            "online"
        } else {
            "offline"
        },
        last_seen: g.last_seen_at.clone(),
        rssi: None,
        snr: None,
        radio: "lorawan",
    }
}

/// Register an external gateway — §7.
///
/// Gated on the gateway-bridge opt-in flag: with the bridge off, port 1700 never opens, so a
/// registered external gateway could never connect. Failing loudly beats registering something
/// that silently cannot work.
///
/// Returns `true` when the EUI was already registered and nothing was created. Registering an
/// already-registered gateway is a no-op, not a failure: without the ask-first lookup the
/// duplicate `Create` comes back as ChirpStack's raw `duplicate key value violates unique
/// constraint "gateway_pkey"` `Internal`, which the taxonomy maps to the *retryable*
/// `Transport` (exit 22) for what is a permanent condition (F8, reproduced live 2026-08-25).
pub async fn add_external(
    client: &dyn ChirpStack,
    tenant_id: &str,
    gateway_eui: &Eui,
    name: &str,
    bridge_enabled: bool,
) -> Result<bool> {
    if !bridge_enabled {
        return Err(Error::new(
            Kind::BridgeDisabled,
            Transport::Lorawan,
            Component::ChirpstackService,
            "chirpstack-gateway-bridge is disabled, so port 1700 is closed and an external \
             gateway could never connect; enable the opt-in flag first",
        ));
    }
    if client.find_gateway(tenant_id, gateway_eui).await?.is_some() {
        return Ok(true);
    }
    client
        .create_gateway(&GatewaySpec {
            gateway_id: gateway_eui.clone(),
            tenant_id: tenant_id.to_string(),
            name: name.to_string(),
            description: String::new(),
        })
        .await?;
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filter_honours_an_empty_prefix() {
        assert_eq!(state_topic_filter("eu868"), "eu868/gateway/+/state/conn");
        assert_eq!(state_topic_filter("eu868/"), "eu868/gateway/+/state/conn");
        // Upstream renders an empty prefix as no prefix at all — not a leading slash.
        assert_eq!(state_topic_filter(""), "gateway/+/state/conn");
    }

    #[test]
    fn the_eui_comes_from_the_path_segment() {
        let got = eui_from_state_topic("eu868/gateway/0011223344556677/state/conn")
            .expect("matches the shape")
            .expect("valid EUI");
        assert_eq!(got.as_str(), "0011223344556677");

        // Empty prefix.
        let got = eui_from_state_topic("gateway/aabbccddeeff0011/state/conn")
            .expect("matches")
            .expect("valid");
        assert_eq!(got.as_str(), "aabbccddeeff0011");
    }

    /// A neighbouring retained topic must never be mistaken for a gateway.
    #[test]
    fn unrelated_topics_do_not_match() {
        for t in [
            "eu868/gateway/0011223344556677/event/stats",
            "eu868/gateway/0011223344556677/state/other",
            "eu868/application/1/device/x/event/up",
            "state/conn",
            "",
        ] {
            assert!(eui_from_state_topic(t).is_none(), "must not match: {t:?}");
        }
    }

    /// A matching shape carrying a malformed EUI is an error, not a skip — silently ignoring
    /// it would look identical to "no radio".
    #[test]
    fn a_malformed_eui_in_a_matching_topic_is_an_error() {
        let r = eui_from_state_topic("eu868/gateway/nothex/state/conn").expect("shape matches");
        assert_eq!(r.expect_err("bad EUI").kind(), Kind::InvalidArgument);
    }

    #[test]
    fn the_online_window_is_300_seconds_and_never_seen_is_offline() {
        let now = parse_rfc3339_unix("2026-08-20T12:00:00Z").expect("parses");
        assert!(
            is_online(Some("2026-08-20T11:56:00Z"), now),
            "240 s ago is online"
        );
        assert!(
            is_online(Some("2026-08-20T11:55:00Z"), now),
            "exactly 300 s is online"
        );
        assert!(
            !is_online(Some("2026-08-20T11:54:59Z"), now),
            "301 s ago is offline"
        );
        assert!(!is_online(None, now), "never seen is offline");
        // Clock skew: a future timestamp counts as just-seen, not as very old.
        assert!(is_online(Some("2026-08-20T12:05:00Z"), now));
    }

    #[test]
    fn an_auth_failure_and_an_unreachable_service_are_different_api_states() {
        let auth = Error::new(
            Kind::Unauthenticated,
            Transport::Lorawan,
            Component::ChirpstackApi,
            "x",
        );
        let down = Error::new(
            Kind::ServiceDown,
            Transport::Lorawan,
            Component::ChirpstackService,
            "x",
        );
        assert_eq!(ApiStatus::from_error(&auth), ApiStatus::AuthFailed);
        assert_eq!(ApiStatus::from_error(&down), ApiStatus::Unreachable);
        // Serialized spellings are the documented ones.
        assert_eq!(
            serde_json::to_value(ApiStatus::AuthFailed).expect("ser"),
            "auth_failed"
        );
        assert_eq!(serde_json::to_value(ApiStatus::Ok).expect("ser"), "ok");
    }

    #[test]
    fn rfc3339_parsing_handles_the_cases_chirpstack_emits() {
        assert_eq!(parse_rfc3339_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_unix("2026-08-20T12:00:00Z"),
            Some(1_787_227_200)
        );
        // Fractional seconds and offsets still parse to the second.
        assert_eq!(
            parse_rfc3339_unix("2026-08-20T12:00:00.123456Z"),
            Some(1_787_227_200)
        );
        assert_eq!(parse_rfc3339_unix("nonsense"), None);
        assert_eq!(parse_rfc3339_unix("2026-13-01T00:00:00Z"), None);
    }

    /// The report `timestamp` and the `last_seen` comparison must agree on what a date is —
    /// they are the same document's two time fields, so they round-trip through each other.
    #[test]
    fn the_timestamp_format_round_trips_through_the_parser() {
        for s in [
            "1970-01-01T00:00:00Z",
            "2000-02-29T23:59:59Z",
            "2026-08-20T12:00:00Z",
            "2099-12-31T00:00:01Z",
        ] {
            let unix = parse_rfc3339_unix(s).expect("parses");
            assert_eq!(format_rfc3339(unix), s, "round trip for {s}");
        }
    }

    /// The clock is only ever read here, so this is the one place a broken RTC could turn a
    /// health poll into a panic.
    #[test]
    fn the_current_time_is_readable_and_formats() {
        let now = now_unix();
        assert!(now > 0, "the test host has a set clock");
        assert_eq!(
            parse_rfc3339_unix(&format_rfc3339(now)),
            Some(now),
            "what the report prints is what the parser reads back"
        );
    }
}

#[cfg(test)]
mod discovery_tests {
    use super::*;
    use crate::lorawan::fake::FakeRetainedTopics;
    use crate::lorawan::fake::{FakeChirpStack, DEFAULT_TENANT_ID};

    /// F20, measured live against 4.19.0 on 2026-08-25: `GatewayService.Get` on an EUI that is
    /// not registered yet answers `Unauthenticated` under every key below admin, so the
    /// ask-first check must not be a `Get`. `add_external` asks through the tenant-scoped
    /// list instead, which answers for all three scopes.
    #[test]
    fn add_external_survives_the_exists_join_on_a_fresh_gateway() {
        let fake = FakeChirpStack::new().with_gateway_get_exists_join();
        let eui = Eui::parse("24e124fffefd3bda").expect("eui");
        let existed = rt()
            .block_on(add_external(&fake, DEFAULT_TENANT_ID, &eui, "UG63", true))
            .expect("a fresh gateway registers even under the EXISTS-join");
        assert!(!existed, "it was not registered before");
        assert_eq!(fake.gateway_count(), 1);
    }

    /// F8: a duplicate registration is a permanent condition, so it must not surface as the
    /// retryable `Transport` (22) that ChirpStack's raw duplicate-key `Internal` maps to.
    #[test]
    fn add_external_is_idempotent_and_never_double_creates() {
        let fake = FakeChirpStack::new().with_gateway_get_exists_join();
        let eui = Eui::parse("24e124fffefd3bda").expect("eui");
        rt().block_on(add_external(&fake, DEFAULT_TENANT_ID, &eui, "UG63", true))
            .expect("first add");
        let existed = rt()
            .block_on(add_external(&fake, DEFAULT_TENANT_ID, &eui, "UG63", true))
            .expect("second add is a no-op, not a duplicate-key Internal");
        assert!(existed, "the second call reports it was already registered");
        assert_eq!(fake.gateway_count(), 1, "no second row");
    }

    /// A broken API must not be read as "not registered" and turned into a blind `Create`.
    #[test]
    fn add_external_propagates_a_failing_lookup_without_creating() {
        let fake = FakeChirpStack::new();
        fake.fail_every_call_with(
            chirpstack_api::tonic::Code::Unavailable,
            "connection refused",
        );
        let eui = Eui::parse("24e124fffefd3bda").expect("eui");
        let err = rt()
            .block_on(add_external(&fake, DEFAULT_TENANT_ID, &eui, "UG63", true))
            .expect_err("the lookup failure propagates");
        assert_eq!(err.kind(), Kind::ServiceDown);
        assert_eq!(fake.gateway_count(), 0, "nothing was created");
        assert!(
            !fake.calls().iter().any(|c| c == "create_gateway"),
            "no create was attempted: {:?}",
            fake.calls()
        );
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn one_retained_state_topic_yields_the_eui() {
        let topics = FakeRetainedTopics::with(&[
            "eu868/gateway/0011223344556677/state/conn",
            // Noise the filter must exclude.
            "eu868/gateway/0011223344556677/event/stats",
            "eu868/application/1/device/aa/event/up",
        ]);
        let got = rt()
            .block_on(discover(&topics, "eu868", DISCOVERY_TIMEOUT))
            .expect("discovers");
        assert_eq!(
            got.map(|e| e.as_str().to_string()),
            Some("0011223344556677".into())
        );
    }

    /// No radio is a degraded success: `None`, not an error. The qemuarm64 smoke image has no
    /// concentrator and must still go green.
    #[test]
    fn no_retained_topic_is_none_not_an_error() {
        let got = rt()
            .block_on(discover(
                &FakeRetainedTopics::empty(),
                "eu868",
                DISCOVERY_TIMEOUT,
            ))
            .expect("must not be an error");
        assert_eq!(got, None);
    }

    /// Two local forwarders is a misconfiguration, not something to pick from: registering
    /// the wrong one silently drops every uplink from the other.
    #[test]
    fn two_gateways_is_an_error_that_names_both() {
        let topics = FakeRetainedTopics::with(&[
            "eu868/gateway/0011223344556677/state/conn",
            "eu868/gateway/aabbccddeeff0011/state/conn",
        ]);
        let err = rt()
            .block_on(discover(&topics, "eu868", DISCOVERY_TIMEOUT))
            .expect_err("ambiguous");
        assert_eq!(err.kind(), Kind::InvalidArgument);
        assert!(err.message().contains("0011223344556677"));
        assert!(err.message().contains("aabbccddeeff0011"));
        assert!(
            err.message().contains("--gateway-eui"),
            "must say how to resolve it"
        );
    }

    /// The same retained topic seen twice is one gateway, not two.
    #[test]
    fn duplicate_topics_collapse() {
        let topics = FakeRetainedTopics::with(&[
            "eu868/gateway/0011223344556677/state/conn",
            "eu868/gateway/0011223344556677/state/conn",
        ]);
        let got = rt()
            .block_on(discover(&topics, "eu868", DISCOVERY_TIMEOUT))
            .expect("discovers");
        assert!(got.is_some());
    }

    /// The prefix is configuration: a Hub whose forwarder renders no prefix still works.
    #[test]
    fn an_empty_prefix_still_discovers() {
        let topics = FakeRetainedTopics::with(&["gateway/0011223344556677/state/conn"]);
        let got = rt()
            .block_on(discover(&topics, "", DISCOVERY_TIMEOUT))
            .expect("discovers");
        assert!(got.is_some());

        // And the wrong prefix finds nothing rather than matching by accident.
        let got = rt()
            .block_on(discover(&topics, "eu868", DISCOVERY_TIMEOUT))
            .expect("no error");
        assert_eq!(got, None);
    }

    /// A broker that cannot be reached is not "no radio" — that distinction decides whether
    /// the boot marker is withheld or the unit fails.
    #[test]
    fn a_broker_failure_is_an_error_not_a_degraded_success() {
        let topics = FakeRetainedTopics::failing(Error::new(
            Kind::ServiceDown,
            Transport::Lorawan,
            Component::MqttBroker,
            "connection refused",
        ));
        let err = rt()
            .block_on(discover(&topics, "eu868", DISCOVERY_TIMEOUT))
            .expect_err("must surface");
        assert_eq!(err.kind(), Kind::ServiceDown);
        assert_eq!(err.component(), Component::MqttBroker);
    }

    #[test]
    fn the_bridge_flag_gates_external_gateway_registration() {
        use crate::lorawan::fake::{FakeChirpStack, DEFAULT_TENANT_ID};
        let cs = FakeChirpStack::new();
        let eui = Eui::parse("0011223344556677").expect("valid");

        let err = rt()
            .block_on(add_external(&cs, DEFAULT_TENANT_ID, &eui, "ext", false))
            .expect_err("bridge disabled");
        assert_eq!(err.kind(), Kind::BridgeDisabled);
        assert_eq!(err.exit_code(), 30);
        assert_eq!(
            cs.gateway_count(),
            0,
            "nothing may be registered when it cannot connect"
        );

        rt().block_on(add_external(&cs, DEFAULT_TENANT_ID, &eui, "ext", true))
            .expect("bridge enabled");
        assert_eq!(cs.gateway_count(), 1);
    }

    fn e(s: &str) -> Eui {
        Eui::parse(s).unwrap()
    }

    /// #169: a Client's or a stale retained gateway next to the local one picks the local one.
    #[test]
    fn a_stale_retained_gateway_yields_to_the_registered_one() {
        let found = vec![e("0016c001f136afa0"), e("0016c001f136c66a")];
        let got = pick(found, &[e("0016c001f136c66a")], "f").unwrap();
        assert_eq!(got, Some(e("0016c001f136c66a")));
    }

    /// Without exactly one registered candidate, several retained gateways still refuse.
    #[test]
    fn several_retained_gateways_and_none_registered_still_refuse() {
        let found = vec![e("0016c001f136afa0"), e("0016c001f136c66a")];
        assert!(pick(found.clone(), &[], "f").is_err());
        assert!(pick(found.clone(), &found, "f").is_err());
    }
}
