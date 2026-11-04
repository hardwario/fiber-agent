//! The `radio/gateways` health poll — `plan/control/radio/plan.md` §6, `lorawan.md` §6.
//!
//! One tick: ask ChirpStack which gateways it knows and when it last heard them, read the
//! forwarder's retained state topics for the local channel's own inventory, and produce the
//! `radio/gateways` document. `proximos-control` runs the tick on a timer and publishes the
//! result; nothing here owns a connection or a clock.
//!
//! # The v1 defect this exists to make unrepresentable
//!
//! v1's `get_gateways_status` returned a gateway list, so a **rejected login reported every
//! gateway offline** — operationally indistinguishable from a site-wide radio outage, and the
//! reason issue #8 exists. §6's fix is structural: `api_status` is a **top-level field, not
//! per-gateway**, and `gateways[]` is *"always the last known state from the local channel …
//! independent of `api_status`, and never erased or replaced by an API auth failure"*.
//!
//! [`HealthPoller`] implements that literally, and the state it holds is the whole reason it
//! is a struct rather than a free function: on a failed poll it republishes the **last known**
//! rows with the new `api_status`, rather than an empty list. A poller that had never
//! succeeded falls back to the retained state topics — which say *which* gateways the local
//! forwarder has registered, and nothing about when they were last heard, so those rows carry
//! `last_seen: null` and `status: "offline"`. That combination is honest: never-seen **is**
//! offline (§6), and a null `last_seen` is visibly different from a stale timestamp.
//!
//! # Degraded-green
//!
//! A Hub with no concentrator (the qemuarm64 smoke image, a P2P-only unit) publishes an empty
//! `gateways[]` with `api_status: ok`. That is a complete, true answer and not an error —
//! `lorawan.md` §6's "no radio is a degraded success" applies to the health poll for the same
//! reason it applies to `create`.

use std::time::Duration;

use crate::lorawan::client::ChirpStack;
use crate::lorawan::gateway::{
    self, ApiStatus, GatewayStatus, GatewaysReport, RetainedTopics, DISCOVERY_TIMEOUT,
};

/// How often the poll runs — `plan.md`'s "poll gateway status every **60 s** (comfortably
/// inside the 300 s online window, §6)".
///
/// Five ticks inside one online window: a gateway that goes quiet is reported offline within
/// a tick of the window closing, and a single failed poll never moves a gateway's state on its
/// own.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// `PROXIMOS_RADIO_HEALTH_INTERVAL_S` shortens the tick.
///
/// For tests and for the integration stack's "`radio/gateways` within one shortened tick"
/// acceptance — **not** a config knob: §6 makes the 300 s online window a named constant
/// rather than configuration, and the poll cadence that has to stay comfortably inside it is
/// the same kind of fact.
pub const INTERVAL_ENV: &str = "PROXIMOS_RADIO_HEALTH_INTERVAL_S";

/// How long the retained-state read waits. The forwarder's topics are retained, so this covers
/// the replay, not a device appearing — [`DISCOVERY_TIMEOUT`] is the budget `create` uses for
/// the first-boot race and is deliberately reused rather than invented again.
pub const RETAINED_READ_TIMEOUT: Duration = DISCOVERY_TIMEOUT;

pub fn interval() -> Duration {
    match std::env::var(INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(s) if s > 0 => Duration::from_secs(s),
        _ => DEFAULT_INTERVAL,
    }
}

/// What one tick needs to reach the outside world.
///
/// Both halves are optional and both are traits, so a tick is testable with neither: a
/// P2P-only Hub has no ChirpStack, and a Hub whose broker is down still owes a document.
pub struct Inputs<'a> {
    pub client: Option<&'a dyn ChirpStack>,
    /// The tenant `create` resolved. `None` means `create` has not run — a legitimate state on
    /// a fresh Hub, and one that must not turn into an error every 60 s.
    pub tenant_id: Option<&'a str>,
    pub topics: Option<&'a dyn RetainedTopics>,
    /// The forwarder's topic prefix — configuration, not a constant (`lorawan.md` §6).
    pub prefix: &'a str,
    /// The name to give a gateway known only from a retained topic.
    pub gateway_name: &'a str,
}

/// One poll, plus the last known state it is obliged to keep.
#[derive(Debug, Default)]
pub struct HealthPoller {
    last_known: Vec<GatewayStatus>,
    unregistered: Vec<String>,
}

impl HealthPoller {
    pub fn new() -> Self {
        HealthPoller::default()
    }

    /// The rows the last successful poll produced. Empty until one has.
    pub fn last_known(&self) -> &[GatewayStatus] {
        &self.last_known
    }

    /// Gateways the last successful poll found in the forwarder's retained `state/conn` but
    /// not in ChirpStack — the "every uplink dropped" pairing. The daemon hands these to
    /// [`crate::lorawan::converge::Converger::adopt_gateway`] so a gateway `create` missed
    /// at boot is registered on the next tick (#156). Empty after a failed poll: only a
    /// successful list says what ChirpStack does not know.
    pub fn unregistered(&self) -> Vec<crate::lorawan::Eui> {
        self.unregistered
            .iter()
            .filter_map(|e| crate::lorawan::Eui::parse(e).ok())
            .collect()
    }

    /// Run one tick.
    ///
    /// Infallible by design: `radio/gateways` is the topic an operator looks at *because*
    /// something is wrong, so a tick that could return `Err` would be a health report that
    /// goes silent exactly when it is needed. Every failure becomes an `api_status` value.
    pub async fn tick(&mut self, inputs: Inputs<'_>) -> GatewaysReport {
        let now = gateway::now_unix();

        // The local channel's own inventory, read first and independently: it is what
        // `gateways[]` falls back to, so it must not be skipped when the API is the thing
        // that is broken.
        let retained = self.retained_euis(&inputs).await;
        self.unregistered.clear();

        let api = match (inputs.client, inputs.tenant_id) {
            (Some(c), Some(tenant)) => match c.list_gateways(tenant).await {
                Ok(gws) => Ok(gws
                    .iter()
                    .map(|g| gateway::to_status(g, now))
                    .collect::<Vec<_>>()),
                Err(e) => Err(ApiStatus::from_error(&e)),
            },
            // No token yet, or no ChirpStack on this Hub. Neither is an auth failure: nothing
            // was rejected, there was simply nothing to ask.
            _ => Err(ApiStatus::Unreachable),
        };

        let (api_status, gateways) = match api {
            Ok(mut rows) => {
                // A gateway the forwarder has registered locally but ChirpStack has never
                // heard of still belongs in the inventory — that pairing is precisely the
                // "gateway not registered, every uplink dropped" state `lorawan.md` §6 warns
                // about, and it is invisible if the row is missing.
                for eui in &retained {
                    if !rows.iter().any(|r| &r.gateway_eui == eui) {
                        self.unregistered.push(eui.clone());
                        rows.push(unheard_row(eui, inputs.gateway_name));
                    }
                }
                rows.sort_by(|a, b| a.gateway_eui.cmp(&b.gateway_eui));
                self.last_known = rows.clone();
                (ApiStatus::Ok, rows)
            }
            Err(status) => {
                // Never erased by an API failure. The last known rows if there are any;
                // otherwise what the local channel alone can say.
                let mut rows = if self.last_known.is_empty() {
                    retained
                        .iter()
                        .map(|e| unheard_row(e, inputs.gateway_name))
                        .collect::<Vec<_>>()
                } else {
                    self.last_known.clone()
                };
                rows.sort_by(|a, b| a.gateway_eui.cmp(&b.gateway_eui));
                (status, rows)
            }
        };

        GatewaysReport {
            timestamp: gateway::format_rfc3339(now),
            api_status,
            gateways,
        }
    }

    /// Every gateway EUI the forwarder has published retained state for.
    ///
    /// Read through the same seam `create`'s discovery uses, and tolerant in the same way: a
    /// broker that cannot be reached yields nothing rather than failing the tick, because the
    /// API half may still have plenty to report.
    async fn retained_euis(&self, inputs: &Inputs<'_>) -> Vec<String> {
        let Some(t) = inputs.topics else {
            return Vec::new();
        };
        let filter = gateway::state_topic_filter(inputs.prefix);
        let Ok(names) = t.retained_topic_names(&filter, RETAINED_READ_TIMEOUT).await else {
            return Vec::new();
        };
        let mut euis: Vec<String> = names
            .iter()
            .filter_map(|n| gateway::eui_from_state_topic(n))
            .filter_map(|r| r.ok())
            .map(|e| e.as_str().to_string())
            .collect();
        euis.sort();
        euis.dedup();
        euis
    }
}

/// A gateway known only from its retained state topic.
///
/// `last_seen: None` rather than a guess: the topic name carries the EUI and nothing else
/// (`lorawan.md` §6 reads the path segment on purpose, so no protobuf decode is needed), so
/// when it was last heard is genuinely unknown. Never-seen is offline (§6), and a null
/// `last_seen` is visibly different from a stale timestamp.
fn unheard_row(eui: &str, name: &str) -> GatewayStatus {
    GatewayStatus {
        gateway_eui: eui.to_string(),
        name: name.to_string(),
        hostname: None,
        ip: None,
        status: "offline",
        last_seen: None,
        rssi: None,
        snr: None,
        radio: "lorawan",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lorawan::fake::{FakeChirpStack, FakeRetainedTopics, DEFAULT_TENANT_ID};
    use crate::lorawan::model::GatewaySpec;
    use crate::lorawan::Eui;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    fn eui(s: &str) -> Eui {
        Eui::parse(s).expect("valid eui")
    }

    async fn register(fake: &FakeChirpStack, id: &str) {
        fake.create_gateway(&GatewaySpec {
            gateway_id: eui(id),
            tenant_id: DEFAULT_TENANT_ID.to_string(),
            name: "ProXimos Gateway".into(),
            description: String::new(),
        })
        .await
        .expect("registers");
    }

    fn inputs<'a>(
        client: Option<&'a dyn ChirpStack>,
        topics: Option<&'a dyn RetainedTopics>,
    ) -> Inputs<'a> {
        Inputs {
            client,
            tenant_id: Some(DEFAULT_TENANT_ID),
            topics,
            prefix: "eu868",
            gateway_name: "ProXimos Gateway",
        }
    }

    #[test]
    fn a_good_poll_reports_ok_and_the_api_rows() {
        let fake = FakeChirpStack::new();
        let mut poller = HealthPoller::new();
        let report = rt().block_on(async {
            register(&fake, "0011223344556677").await;
            poller.tick(inputs(Some(&fake), None)).await
        });
        assert_eq!(report.api_status, ApiStatus::Ok);
        assert_eq!(report.gateways.len(), 1);
        assert_eq!(report.gateways[0].gateway_eui, "0011223344556677");
        assert!(report.timestamp.ends_with('Z'));
    }

    /// The v1 defect, made unrepresentable: a rejected credential must not read as a
    /// site-wide radio outage. `api_status` says what is wrong and `gateways[]` survives.
    #[test]
    fn an_auth_failure_keeps_the_last_known_rows_and_is_not_offline() {
        let fake = FakeChirpStack::new();
        let mut poller = HealthPoller::new();
        let (good, bad) = rt().block_on(async {
            register(&fake, "0011223344556677").await;
            let good = poller.tick(inputs(Some(&fake), None)).await;
            fake.fail_every_call_with(
                chirpstack_api::tonic::Code::Unauthenticated,
                "token rejected",
            );
            let bad = poller.tick(inputs(Some(&fake), None)).await;
            (good, bad)
        });

        assert_eq!(good.api_status, ApiStatus::Ok);
        assert_eq!(bad.api_status, ApiStatus::AuthFailed);
        assert_eq!(
            bad.gateways, good.gateways,
            "the gateway list is never erased or replaced by an API auth failure (§6)"
        );
        assert!(!bad.gateways.is_empty(), "not an empty every-gateway-down");
    }

    /// An auth failure and an unreachable service are different operator actions, so they stay
    /// different values even though both keep the rows.
    #[test]
    fn an_unreachable_api_is_distinguishable_from_a_rejected_credential() {
        let fake = FakeChirpStack::new();
        let mut poller = HealthPoller::new();
        let (auth, down, none) = rt().block_on(async {
            register(&fake, "0011223344556677").await;
            poller.tick(inputs(Some(&fake), None)).await;

            fake.fail_every_call_with(chirpstack_api::tonic::Code::Unauthenticated, "rejected");
            let auth = poller.tick(inputs(Some(&fake), None)).await;

            fake.fail_every_call_with(chirpstack_api::tonic::Code::Unavailable, "refused");
            let down = poller.tick(inputs(Some(&fake), None)).await;

            // No client at all — a P2P-only Hub. Nothing was rejected; there was nothing to
            // ask, so this is unreachable and never auth_failed.
            let none = poller.tick(inputs(None, None)).await;
            (auth, down, none)
        });

        assert_eq!(auth.api_status, ApiStatus::AuthFailed);
        assert_eq!(down.api_status, ApiStatus::Unreachable);
        assert_eq!(none.api_status, ApiStatus::Unreachable);
        assert_eq!(none.gateways.len(), 1, "still the last known row");
    }

    /// With no successful poll behind it, the local channel is all there is — and it can only
    /// say *which* gateways exist, never when they were heard.
    #[test]
    fn a_first_poll_that_fails_falls_back_to_the_retained_topics() {
        let fake = FakeChirpStack::new();
        fake.fail_every_call_with(chirpstack_api::tonic::Code::Unauthenticated, "rejected");
        let topics = FakeRetainedTopics::with(&[
            "eu868/gateway/24e124fffefd3bda/state/conn",
            "eu868/gateway/0011223344556677/state/conn",
            // Noise the filter must exclude.
            "eu868/gateway/0011223344556677/event/stats",
        ]);
        let mut poller = HealthPoller::new();
        let report = rt().block_on(poller.tick(inputs(Some(&fake), Some(&topics))));

        assert_eq!(report.api_status, ApiStatus::AuthFailed);
        let euis: Vec<&str> = report
            .gateways
            .iter()
            .map(|g| g.gateway_eui.as_str())
            .collect();
        assert_eq!(euis, vec!["0011223344556677", "24e124fffefd3bda"]);
        for g in &report.gateways {
            assert_eq!(g.status, "offline", "never-seen is offline (§6)");
            assert_eq!(
                g.last_seen, None,
                "a null last_seen is visibly different from a stale one"
            );
        }
    }

    /// A gateway the forwarder has registered locally but ChirpStack does not know is the
    /// "every uplink dropped with RxInfo set is empty" state — invisible if the row is missing.
    #[test]
    fn a_locally_known_gateway_missing_from_chirpstack_still_appears() {
        let fake = FakeChirpStack::new();
        let topics = FakeRetainedTopics::with(&[
            "eu868/gateway/24e124fffefd3bda/state/conn",
            "eu868/gateway/0011223344556677/state/conn",
        ]);
        let mut poller = HealthPoller::new();
        let report = rt().block_on(async {
            register(&fake, "0011223344556677").await;
            poller.tick(inputs(Some(&fake), Some(&topics))).await
        });

        assert_eq!(report.api_status, ApiStatus::Ok);
        let euis: Vec<&str> = report
            .gateways
            .iter()
            .map(|g| g.gateway_eui.as_str())
            .collect();
        assert_eq!(euis, vec!["0011223344556677", "24e124fffefd3bda"]);
        let unregistered = report
            .gateways
            .iter()
            .find(|g| g.gateway_eui == "24e124fffefd3bda")
            .expect("present");
        assert_eq!(unregistered.last_seen, None);
    }

    /// A Hub with no concentrator publishes a complete, true, empty answer — not an error.
    #[test]
    fn no_radio_is_a_green_empty_report() {
        let fake = FakeChirpStack::new();
        let mut poller = HealthPoller::new();
        let report =
            rt().block_on(poller.tick(inputs(Some(&fake), Some(&FakeRetainedTopics::empty()))));
        assert_eq!(report.api_status, ApiStatus::Ok);
        assert!(report.gateways.is_empty());
    }

    /// A broker that is down must not fail the tick — the API half may have plenty to say.
    #[test]
    fn a_broker_failure_does_not_take_the_tick_down() {
        let fake = FakeChirpStack::new();
        let topics = FakeRetainedTopics::failing(crate::error::Error::new(
            crate::error::Kind::ServiceDown,
            crate::error::Transport::Lorawan,
            crate::error::Component::MqttBroker,
            "connection refused",
        ));
        let mut poller = HealthPoller::new();
        let report = rt().block_on(async {
            register(&fake, "0011223344556677").await;
            poller.tick(inputs(Some(&fake), Some(&topics))).await
        });
        assert_eq!(report.api_status, ApiStatus::Ok);
        assert_eq!(report.gateways.len(), 1);
    }

    /// A fresh Hub where `create` has not run has no tenant. Every 60 s that must be a report,
    /// not an error.
    #[test]
    fn no_tenant_yet_is_unreachable_rather_than_a_failure() {
        let fake = FakeChirpStack::new();
        let mut poller = HealthPoller::new();
        let report = rt().block_on(poller.tick(Inputs {
            client: Some(&fake),
            tenant_id: None,
            topics: None,
            prefix: "eu868",
            gateway_name: "ProXimos Gateway",
        }));
        assert_eq!(report.api_status, ApiStatus::Unreachable);
        assert!(report.gateways.is_empty());
    }

    /// 60 s is five ticks inside §6's 300 s online window, and the override exists for tests
    /// and the integration stack rather than as a config knob.
    #[test]
    fn the_interval_is_sixty_seconds_and_well_inside_the_online_window() {
        assert_eq!(DEFAULT_INTERVAL, Duration::from_secs(60));
        assert!(
            DEFAULT_INTERVAL * 4 < gateway::GATEWAY_ONLINE_WINDOW,
            "several ticks must fit inside one online window"
        );
        // The env var is read, not cached at startup, so the integration stack can shorten it.
        assert_eq!(INTERVAL_ENV, "PROXIMOS_RADIO_HEALTH_INTERVAL_S");
        assert_eq!(interval(), DEFAULT_INTERVAL, "unset in this process");
    }

    /// The report is the §6 document, key for key.
    #[test]
    fn the_published_document_is_the_documented_shape() {
        let fake = FakeChirpStack::new();
        let mut poller = HealthPoller::new();
        let report = rt().block_on(async {
            register(&fake, "0011223344556677").await;
            poller.tick(inputs(Some(&fake), None)).await
        });
        let v = serde_json::to_value(&report).expect("serializes");
        assert!(v.get("timestamp").is_some());
        assert_eq!(v["api_status"], "ok");
        assert_eq!(v["gateways"][0]["gateway_eui"], "0011223344556677");
        assert_eq!(v["gateways"][0]["radio"], "lorawan");
        assert!(v["gateways"][0].get("status").is_some());
    }
}
