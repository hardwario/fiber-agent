//! MQTT topic namespace for the FIBER protocol.
//!
//! Mirrors `viewer/backend/app/mqtt/topics.py::TopicBuilder` and
//! `get_subscribe_topics()`. See `mqtt.md` "Transport & conventions" and
//! "Subscriptions" sections.
//!
//! Convention: `{prefix}/{hostname}/{area}[/{sub}…]`, hostname always
//! segment 2. Unlike v1 (mqtt.md quirk #2, "Prefix is a lie"), hostname
//! extraction here honours the configured prefix instead of hardcoding
//! `fiber`.

/// The one outbound command topic suffix for every verb — there is a
/// single dialect and a single topic (design.md §7). Published as
/// `{prefix}/{hostname}/{COMMANDS_CONFIG_REQUEST}`.
pub const COMMANDS_CONFIG_REQUEST: &str = "commands/config/request";
/// The confirm half of the challenge/confirm handshake. `manage` and above
/// only — `read`/`operate` are single-shot and never confirm (design.md
/// §5).
pub const COMMANDS_CONFIG_CONFIRM: &str = "commands/config/confirm";
/// Pairing, the one outbound topic that is not a command.
pub const PAIR_REQUEST: &str = "pair/request";

/// The device asking this portal to pair with it (GitLab #61). **Inbound** — the only pairing
/// topic that is — and covered by the `{prefix}/+/pair/#` subscription already in
/// [`TopicBuilder::get_subscribe_topics`], so adding it changed no subscribe list.
pub const PAIR_INVITE: &str = "pair/invite";

/// Builds subscribe/publish topics for a given topic prefix.
///
/// v1 built a `TopicBuilder` bound to one hostname (for publishes) with
/// wildcard subscriptions built separately from the same prefix. This
/// version keeps hostname as an explicit per-call argument since the
/// v2 portal tracks a fleet from one builder instance.
pub struct TopicBuilder {
    prefix: String,
}

impl TopicBuilder {
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
        }
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Subscribe topics in the exact load-bearing v1 order (mqtt.md
    /// "Subscriptions"): `+/status` MUST be last (retained-replay
    /// protection — several handlers set `online=true` as a side effect,
    /// so a stale retained config/sensor message must not reprocess after
    /// a retained "offline" status). `power/standby` may precede `status`
    /// because `update_standby()` never touches `online`/`last_seen`.
    pub fn get_subscribe_topics(&self) -> Vec<String> {
        let p = &self.prefix;
        vec![
            format!("{p}/+/sensors/aggregated"),
            format!("{p}/+/sensors/summary"),
            format!("{p}/+/system/info"),
            format!("{p}/+/alarms/events"),
            format!("{p}/+/errors"),
            format!("{p}/+/responses/#"),
            format!("{p}/+/config/#"),
            format!("{p}/+/pair/#"),
            format!("{p}/+/lorawan/sensors"),
            format!("{p}/+/lorawan/sensors/+/config"),
            format!("{p}/+/lorawan/sensors/+/history"),
            format!("{p}/+/lorawan/sensors/+/info"),
            format!("{p}/+/lorawan/sensors/+/full-config"),
            format!("{p}/+/lorawan/sensors/+/command"),
            format!("{p}/+/lorawan/gateways"),
            format!("{p}/+/eye/sensors"),
            format!("{p}/+/eye/aggregated"),
            // Beacon archive download progress and rows (beacon_history.md §2.2-2.3).
            format!("{p}/+/eye/history"),
            format!("{p}/+/eye/detect"),
            // The cluster's Beacon registry and home Hubs, and the per-Hub views
            // (decision #42). `eye/cluster` is the Hubs' own and is not ingested.
            format!("{p}/+/eye/homes"),
            format!("{p}/+/eye/cluster"),
            format!("{p}/+/export/+/+"),
            format!("{p}/+/export/+/+/+"),
            format!("{p}/+/power/standby"),
            format!("{p}/+/power/events/standby"),
            format!("{p}/+/status"),
        ]
    }

    // ---- Outbound (portal -> device): none ----
    //
    // There is one command topic and one dialect (design.md §7), so the
    // fifteen per-verb builders that used to live here are gone with the
    // flat dialect they addressed — including `restart_device`, whose topic
    // nothing on the device ever subscribed to. Everything a command needs
    // is [`COMMANDS_CONFIG_REQUEST`] / [`COMMANDS_CONFIG_CONFIRM`] /
    // [`PAIR_REQUEST`], and the one place that formats them is the relay.
    //
    // Three builders for those three constants survived P2 with no caller.
    // They are gone too: §7's rule is "deleted **or** the one topic source —
    // not both", and a second way to spell a command topic is what
    // `no_command_topic_is_formatted_outside_this_module` cannot see, since
    // it looks for `format!` in `portal/src/**` and a builder call is
    // neither.
    //
    // `TopicBuilder` is now inbound only: the subscribe list, the prefix,
    // and `extract_hostname`.
}

/// Extract the hostname (topic segment 2) from an inbound topic, honouring
/// `prefix` instead of hardcoding `fiber` (mqtt.md quirk #2: v1's
/// `extract_hostname_from_topic` regex hardcodes the literal `fiber`
/// regardless of `topic_prefix`).
///
/// Returns `None` if the topic doesn't start with `{prefix}/` or has no
/// second segment.
pub fn extract_hostname<'a>(topic: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = topic.strip_prefix(prefix)?.strip_prefix('/')?;
    let hostname = rest.split('/').next()?;
    if hostname.is_empty() {
        None
    } else {
        Some(hostname)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_subscribed_last() {
        let tb = TopicBuilder::new("proximos");
        let topics = tb.get_subscribe_topics();
        assert_eq!(topics.last().unwrap(), "proximos/+/status");
        // power/standby immediately precedes it (retained-replay note).
        let status_idx = topics.len() - 1;
        assert_eq!(topics[status_idx - 1], "proximos/+/power/events/standby");
        assert_eq!(topics[status_idx - 2], "proximos/+/power/standby");
    }

    #[test]
    fn subscribe_topic_count_and_order() {
        let tb = TopicBuilder::new("proximos");
        let topics = tb.get_subscribe_topics();
        // One entry per row in mqtt.md's table, plus `sensors/summary` (the live
        // 1-Wire sample) and `eye/aggregated` (plan/control/storage_intervals.md §4, §5).
        assert_eq!(topics.len(), 26);
        assert_eq!(topics[0], "proximos/+/sensors/aggregated");
        assert_eq!(topics[1], "proximos/+/sensors/summary");
        assert_eq!(topics[2], "proximos/+/system/info");
        assert_eq!(topics[3], "proximos/+/alarms/events");
        assert_eq!(topics[4], "proximos/+/errors");
        assert_eq!(topics[5], "proximos/+/responses/#");
        assert_eq!(topics[6], "proximos/+/config/#");
        assert_eq!(topics[7], "proximos/+/pair/#");
        assert_eq!(
            topics.last().unwrap(),
            "proximos/+/status",
            "status stays last"
        );
    }

    #[test]
    fn extract_hostname_default_prefix() {
        assert_eq!(
            extract_hostname("proximos/FIBER-001/sensors/aggregated", "proximos"),
            Some("FIBER-001")
        );
    }

    #[test]
    fn extract_hostname_non_default_prefix() {
        assert_eq!(
            extract_hostname("acme/FIBER-001/system/info", "acme"),
            Some("FIBER-001")
        );
        // Wrong prefix must not match, unlike v1's hardcoded "fiber" regex.
        assert_eq!(
            extract_hostname("acme/FIBER-001/system/info", "proximos"),
            None
        );
    }

    #[test]
    fn extract_hostname_missing_segment() {
        assert_eq!(extract_hostname("proximos/", "proximos"), None);
        assert_eq!(extract_hostname("proximos", "proximos"), None);
        assert_eq!(extract_hostname("other/host/status", "proximos"), None);
    }
}
