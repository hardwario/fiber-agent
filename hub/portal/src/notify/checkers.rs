//! System-alert checkers (ws.md "Outbound notifications" §Email, last
//! bullet): pure evaluation of one device's system snapshot against the
//! configured thresholds, with in-memory onset debounce — one event per
//! condition onset, re-armed when the condition clears, reset by a portal
//! restart (the state is not persisted, v1 parity).
//!
//! One [`SystemAlertState`] per device — the caller keeps a
//! `hostname -> SystemAlertState` map and calls [`SystemAlertState::evaluate`]
//! whenever fresh telemetry lands.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use super::payload::{email_time, tr_with, EmailBody, HEADER_CRITICAL};

/// The six v1 system-alert kinds, plus the two per-device kinds B2 adds
/// (devices.backend.md §5), the backfill terminal kind B3 adds (§6.3) and
/// the three calibration reminders (calibration.cs.md §3, amended
/// 2026-10-13).
/// `as_str` matches the v1 `system_alert_history.event_type` values for the
/// original six.
///
/// [`Self::NodeOffline`]/[`Self::BeaconOffline`] sit **beside**
/// [`Self::DeviceOffline`] rather than replacing it: that one is Hub-scoped
/// and answers "is the Hub reachable", which stays true and useful while a
/// single Node behind a perfectly healthy Hub has gone quiet. The device's
/// identity rides in the event `message` — `email_body` renders that under
/// `Message`, with `Device` still naming the Hub it is behind, which is
/// where an operator would go looking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SystemAlertKind {
    BatteryLow,
    StorageLow,
    PortalStorageLow,
    NetworkDisconnect,
    DeviceOffline,
    VersionMismatch,
    /// One Node silent for longer than K x its `interval_report`.
    NodeOffline,
    /// One Beacon silent for longer than K x its effective store interval.
    BeaconOffline,
    /// One Node's history backfill exhausted its retries — the portal asked
    /// three times over three of the Node's own reporting cadences and got
    /// nothing back, so the window is not coming (devices.backend.md §6.3,
    /// B3). Distinct from [`Self::NodeOffline`]: a Node can be reporting
    /// live perfectly well and still refuse to replay its flash.
    NodeHistoryUnavailable,
    /// One Node's or Beacon's calibration certificate entered its first
    /// warning window (plan/portal/calibration.cs.md §3, amended
    /// 2026-10-13). Sent once, on the onset of the calibration alarm.
    CalibrationDue,
    /// The same certificate entered its second warning window
    /// (`second_warning_days`) — the second e-mail; the alarm stays a
    /// warning. Also the first one, when an upload or a date edit lands
    /// straight inside that window.
    CalibrationSecondWarning,
    /// The certificate's expiry date has come and the alarm turned
    /// critical, including an alarm that opens critical because the
    /// certificate was already that late.
    CalibrationExpired,
    /// A Hub fell behind the configuration the portal holds for it — the
    /// onset of the `sync:<hub>` alarm (plan/portal/config_sync.md §10).
    ConfigSyncBehind,
}

impl SystemAlertKind {
    /// History/event-type identifier (v1 parity).
    pub fn as_str(self) -> &'static str {
        match self {
            SystemAlertKind::BatteryLow => "battery_low",
            SystemAlertKind::StorageLow => "storage_low",
            // Persisted event_type value, inherited from v1 — do not rename
            // even though the variant is now `PortalStorageLow` (renaming
            // the string would orphan existing DB rows and break subscribers).
            SystemAlertKind::PortalStorageLow => "viewer_storage_low",
            SystemAlertKind::NetworkDisconnect => "network_disconnect",
            SystemAlertKind::DeviceOffline => "device_offline",
            SystemAlertKind::VersionMismatch => "version_mismatch",
            // New in v2 — no v1 rows to stay compatible with, so these are
            // named for what they are rather than inheriting a quirk.
            SystemAlertKind::NodeOffline => "node_offline",
            SystemAlertKind::BeaconOffline => "beacon_offline",
            SystemAlertKind::NodeHistoryUnavailable => "node_history_unavailable",
            SystemAlertKind::CalibrationDue => "calibration_due",
            SystemAlertKind::CalibrationSecondWarning => "calibration_second_warning",
            SystemAlertKind::CalibrationExpired => "calibration_expired",
            SystemAlertKind::ConfigSyncBehind => "config_sync_behind",
        }
    }

    /// Human title — the `{Event Type}` part of the email subject.
    pub fn title(self) -> &'static str {
        match self {
            SystemAlertKind::BatteryLow => "Battery Low",
            SystemAlertKind::StorageLow => "Storage Low",
            SystemAlertKind::PortalStorageLow => "Portal Storage Low",
            SystemAlertKind::NetworkDisconnect => "Network Disconnect",
            SystemAlertKind::DeviceOffline => "Device Offline",
            SystemAlertKind::VersionMismatch => "Version Mismatch",
            SystemAlertKind::NodeOffline => "Node Offline",
            SystemAlertKind::BeaconOffline => "Beacon Offline",
            SystemAlertKind::NodeHistoryUnavailable => "Node History Unavailable",
            SystemAlertKind::CalibrationDue => "Calibration Due",
            SystemAlertKind::CalibrationSecondWarning => "Calibration Second Warning",
            SystemAlertKind::CalibrationExpired => "Calibration Expired",
            SystemAlertKind::ConfigSyncBehind => "Configuration Sync Behind",
        }
    }
}

/// One fired alert (condition onset). `message` is the English sentence
/// with the observed values — what the delivery history records and logs
/// show. The e-mail is written from `key`'s catalogue entry
/// (`alarms.email.system.msg.<key>`) with `params` filled, in the
/// installation's language; `message` is its fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemAlertEvent {
    pub kind: SystemAlertKind,
    pub message: String,
    pub key: &'static str,
    pub params: Vec<(&'static str, String)>,
}

impl SystemAlertEvent {
    /// An alert whose e-mail text is the catalogue entry `key` with `params`.
    pub fn new(
        kind: SystemAlertKind,
        message: String,
        key: &'static str,
        params: Vec<(&'static str, String)>,
    ) -> Self {
        Self {
            kind,
            message,
            key,
            params,
        }
    }

    /// The event's name in `lang`.
    fn title_in(&self, lang: &str) -> String {
        tr_with(
            lang,
            &format!("alarms.email.system.title.{}", self.kind.as_str()),
            &[],
        )
    }

    /// The observed-values sentence in `lang`, or the English `message`
    /// when the catalogue has no entry for `key`.
    fn message_in(&self, lang: &str) -> String {
        let key = format!("alarms.email.system.msg.{}", self.key);
        let params: Vec<(&str, &str)> = self.params.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let text = tr_with(lang, &key, &params);
        if text == key {
            self.message.clone()
        } else {
            text
        }
    }

    /// v1 subject: `[SYSTEM ALERT] {hostname} - {Event Type}`, in `lang`.
    pub fn email_subject(&self, hostname: &str, lang: &str) -> String {
        tr_with(
            lang,
            "alarms.email.subject.system",
            &[("hostname", hostname), ("event", &self.title_in(lang))],
        )
    }

    /// The e-mail body, linking to the Hub page when the installation has a
    /// `public_url` (UX-027). Unlike v1 (quirk #8), the HTML part escapes
    /// `hostname` and `message`: both can be MQTT-sourced strings.
    pub fn email_body(&self, hostname: &str, lang: &str, public_url: Option<&str>) -> EmailBody {
        let t = |key: &str| tr_with(lang, key, &[]);
        EmailBody {
            color: HEADER_CRITICAL,
            title: t("alarms.email.title.system"),
            rows: vec![
                (t("alarms.email.row.device"), hostname.to_string()),
                (t("alarms.email.row.event"), self.title_in(lang)),
                (t("alarms.email.row.message"), self.message_in(lang)),
                (
                    t("alarms.email.row.detected"),
                    email_time(time::OffsetDateTime::now_utc(), lang),
                ),
            ],
            link: public_url.map(|base| {
                (
                    t("alarms.email.link.hub"),
                    format!(
                        "{base}/hubs/{}",
                        crate::routes::percent_encode_query(hostname)
                    ),
                )
            }),
            blocks: Vec::new(),
        }
    }
}

/// One device's system telemetry, as far as the checkers care. `None`
/// means "value unknown right now" — unknown never fires an alert and
/// re-arms the corresponding debounce.
#[derive(Debug, Clone, Default)]
pub struct SystemSnapshot {
    pub battery_pct: Option<f64>,
    pub storage_used_pct: Option<f64>,
    /// Fill level of the portal's own data partition.
    pub portal_disk_pct: Option<f64>,
    /// Both network links (wifi + ethernet) down.
    pub network_down: bool,
    pub has_internet: bool,
    pub online: bool,
    pub version: Option<String>,
}

/// Global system-alert config (v1 `system_alert_config`). Defaults match
/// v1: battery ≤ 20, storage ≥ 90, everything enabled except the version
/// check.
#[derive(Debug, Clone)]
pub struct SystemAlertConfig {
    pub battery_enabled: bool,
    pub battery_threshold: f64,
    pub storage_enabled: bool,
    pub storage_threshold: f64,
    pub portal_storage_enabled: bool,
    /// Percent. R-5 (review of !102): once measured (HN-18) it follows the operator's device
    /// storage setting, since the Portal's data sits on the same `/data`; the fixed 70 % of v1
    /// quirk #5 fired with no way to change or silence it.
    pub portal_storage_threshold: f64,
    pub network_enabled: bool,
    pub offline_enabled: bool,
    /// Off by default (v1 parity).
    pub version_enabled: bool,
    pub expected_version: Option<String>,
}

impl Default for SystemAlertConfig {
    fn default() -> Self {
        Self {
            battery_enabled: true,
            battery_threshold: 20.0,
            storage_enabled: true,
            storage_threshold: 90.0,
            portal_storage_enabled: true,
            portal_storage_threshold: PORTAL_DISK_THRESHOLD,
            network_enabled: true,
            offline_enabled: true,
            version_enabled: false,
            expected_version: None,
        }
    }
}

/// The portal's own partition threshold is pinned at 70% — v1 quirk #5:
/// `viewer_storage_threshold` was read by this check but no column/API
/// ever provided it.
const PORTAL_DISK_THRESHOLD: f64 = 70.0;

/// Per-device debounce state: which conditions are currently "on" (an
/// event was emitted and the condition hasn't cleared since).
#[derive(Debug, Default)]
pub struct SystemAlertState {
    active: HashSet<SystemAlertKind>,
}

/// The Hub-scoped conditions currently on, per hostname: the notifier's
/// [`SystemAlertState`] published for synchronous readers — the Hubs list
/// and its Poruchy tile (devices.hub.md §2.1, "Poruchy = Huby s aktivní
/// provozní poruchou"). The notifier is the one place that knows the
/// configured thresholds and which checks are enabled, so the page reads its
/// verdict instead of re-deriving one. Empty until the first `system/info`
/// after a restart, the same as the debounce state it mirrors.
#[derive(Debug, Clone, Default)]
pub struct HubFaults(Arc<RwLock<HashMap<String, Vec<SystemAlertKind>>>>);

impl HubFaults {
    pub fn set(&self, hostname: &str, kinds: Vec<SystemAlertKind>) {
        let mut map = self.0.write().unwrap_or_else(|poison| poison.into_inner());
        if kinds.is_empty() {
            map.remove(hostname);
        } else {
            map.insert(hostname.to_string(), kinds);
        }
    }

    /// The conditions on for `hostname` right now, in the order they were evaluated.
    pub fn kinds(&self, hostname: &str) -> Vec<SystemAlertKind> {
        self.0
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(hostname)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether `hostname` has any condition on right now.
    pub fn any(&self, hostname: &str) -> bool {
        self.0
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains_key(hostname)
    }
}

impl SystemAlertState {
    /// The conditions currently on — what [`HubFaults`] publishes.
    pub fn active(&self) -> Vec<SystemAlertKind> {
        self.active.iter().copied().collect()
    }

    /// Evaluates every checker against `snapshot`; returns the events for
    /// conditions that just transitioned false→true (onset only). A
    /// condition observed false (or unknown, or disabled) re-arms its
    /// debounce so the next onset fires again.
    pub fn evaluate(
        &mut self,
        snapshot: &SystemSnapshot,
        cfg: &SystemAlertConfig,
    ) -> Vec<SystemAlertEvent> {
        // (kind, enabled, condition on as (English message, catalogue key, params))
        type On = Option<(String, &'static str, Vec<(&'static str, String)>)>;
        let pct_alert = |pct: f64, threshold: f64, english: &str, key: &'static str| -> On {
            Some((
                format!("{english} {pct:.1}% (threshold {threshold:.0}%)"),
                key,
                vec![
                    ("pct", format!("{pct:.1}")),
                    ("threshold", format!("{threshold:.0}")),
                ],
            ))
        };
        let checks: [(SystemAlertKind, bool, On); 6] = [
            (
                SystemAlertKind::BatteryLow,
                cfg.battery_enabled,
                snapshot
                    .battery_pct
                    .filter(|pct| *pct <= cfg.battery_threshold)
                    .and_then(|pct| {
                        pct_alert(
                            pct,
                            cfg.battery_threshold,
                            "Battery level is",
                            "battery_low",
                        )
                    }),
            ),
            (
                SystemAlertKind::StorageLow,
                cfg.storage_enabled,
                snapshot
                    .storage_used_pct
                    .filter(|pct| *pct >= cfg.storage_threshold)
                    .and_then(|pct| {
                        pct_alert(
                            pct,
                            cfg.storage_threshold,
                            "Storage usage is",
                            "storage_low",
                        )
                    }),
            ),
            (
                SystemAlertKind::PortalStorageLow,
                cfg.portal_storage_enabled,
                snapshot
                    .portal_disk_pct
                    .filter(|pct| *pct >= cfg.portal_storage_threshold)
                    .and_then(|pct| {
                        pct_alert(
                            pct,
                            cfg.portal_storage_threshold,
                            "Portal data partition usage is",
                            "portal_storage_low",
                        )
                    }),
            ),
            (
                SystemAlertKind::NetworkDisconnect,
                cfg.network_enabled,
                (snapshot.network_down || !snapshot.has_internet).then(|| {
                    if snapshot.network_down {
                        (
                            "All network links are down".to_string(),
                            "network_down",
                            Vec::new(),
                        )
                    } else {
                        (
                            "Device has no internet connectivity".to_string(),
                            "no_internet",
                            Vec::new(),
                        )
                    }
                }),
            ),
            (
                SystemAlertKind::DeviceOffline,
                cfg.offline_enabled,
                (!snapshot.online).then(|| {
                    (
                        "Device is offline".to_string(),
                        "device_offline",
                        Vec::new(),
                    )
                }),
            ),
            (
                SystemAlertKind::VersionMismatch,
                cfg.version_enabled,
                match (&cfg.expected_version, &snapshot.version) {
                    (Some(expected), Some(actual)) if expected != actual => Some((
                        format!("Firmware version is {actual}, expected {expected}"),
                        "version_mismatch",
                        vec![("actual", actual.clone()), ("expected", expected.clone())],
                    )),
                    _ => None,
                },
            ),
        ];

        let mut events = Vec::new();
        for (kind, enabled, condition) in checks {
            match condition.filter(|_| enabled) {
                Some((message, key, params)) => {
                    // Onset only: HashSet::insert is true on first entry.
                    if self.active.insert(kind) {
                        events.push(SystemAlertEvent::new(kind, message, key, params));
                    }
                }
                None => {
                    // Cleared / unknown / disabled → re-arm.
                    self.active.remove(&kind);
                }
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HN-9 (night test 2026-09-26): every system alert kind has an event-type name in both
    /// catalogues; the notification history rendered the raw key and warned on every row.
    #[test]
    fn every_system_alert_kind_has_an_event_type_name() {
        for kind in [
            SystemAlertKind::BatteryLow,
            SystemAlertKind::StorageLow,
            SystemAlertKind::PortalStorageLow,
            SystemAlertKind::NetworkDisconnect,
            SystemAlertKind::DeviceOffline,
            SystemAlertKind::VersionMismatch,
            SystemAlertKind::NodeOffline,
            SystemAlertKind::BeaconOffline,
            SystemAlertKind::NodeHistoryUnavailable,
            SystemAlertKind::CalibrationDue,
            SystemAlertKind::CalibrationSecondWarning,
            SystemAlertKind::CalibrationExpired,
            SystemAlertKind::ConfigSyncBehind,
        ] {
            let key = format!("alarms.event_type.{}", kind.as_str());
            for lang in ["cs", "en"] {
                assert_ne!(crate::i18n::lookup(lang, &key), key, "{lang}: {key}");
            }
        }
    }

    fn healthy() -> SystemSnapshot {
        SystemSnapshot {
            battery_pct: Some(80.0),
            storage_used_pct: Some(40.0),
            portal_disk_pct: Some(30.0),
            network_down: false,
            has_internet: true,
            online: true,
            version: Some("1.2.3".into()),
        }
    }

    fn kinds(events: &[SystemAlertEvent]) -> Vec<SystemAlertKind> {
        events.iter().map(|e| e.kind).collect()
    }

    #[test]
    fn healthy_snapshot_fires_nothing() {
        let mut state = SystemAlertState::default();
        assert!(state
            .evaluate(&healthy(), &SystemAlertConfig::default())
            .is_empty());
    }

    #[test]
    fn onset_debounce_and_rearm_per_kind() {
        let mut state = SystemAlertState::default();
        let cfg = SystemAlertConfig::default();

        let mut snap = healthy();
        snap.battery_pct = Some(15.0);

        // Onset fires once…
        let events = state.evaluate(&snap, &cfg);
        assert_eq!(kinds(&events), vec![SystemAlertKind::BatteryLow]);
        assert!(events[0].message.contains("15.0%"));
        // …repeat evaluation stays silent…
        assert!(state.evaluate(&snap, &cfg).is_empty());
        // …clearing re-arms…
        snap.battery_pct = Some(55.0);
        assert!(state.evaluate(&snap, &cfg).is_empty());
        // …and the next onset fires again.
        snap.battery_pct = Some(10.0);
        assert_eq!(
            kinds(&state.evaluate(&snap, &cfg)),
            vec![SystemAlertKind::BatteryLow]
        );
    }

    #[test]
    fn debounce_is_independent_per_kind() {
        let mut state = SystemAlertState::default();
        let cfg = SystemAlertConfig::default();

        let mut snap = healthy();
        snap.battery_pct = Some(5.0);
        assert_eq!(
            kinds(&state.evaluate(&snap, &cfg)),
            vec![SystemAlertKind::BatteryLow]
        );

        // Battery still low (debounced), storage newly over threshold.
        snap.storage_used_pct = Some(95.0);
        assert_eq!(
            kinds(&state.evaluate(&snap, &cfg)),
            vec![SystemAlertKind::StorageLow]
        );
    }

    #[test]
    fn every_checker_fires_on_its_condition() {
        let cfg = SystemAlertConfig {
            version_enabled: true,
            expected_version: Some("2.0.0".into()),
            ..SystemAlertConfig::default()
        };
        let snap = SystemSnapshot {
            battery_pct: Some(20.0),      // == threshold → fires (≤)
            storage_used_pct: Some(90.0), // == threshold → fires (≥)
            portal_disk_pct: Some(70.0),  // == fixed 70 → fires (≥)
            network_down: true,
            has_internet: false,
            online: false,
            version: Some("1.9.9".into()),
        };
        let mut state = SystemAlertState::default();
        let events = state.evaluate(&snap, &cfg);
        assert_eq!(
            kinds(&events),
            vec![
                SystemAlertKind::BatteryLow,
                SystemAlertKind::StorageLow,
                SystemAlertKind::PortalStorageLow,
                SystemAlertKind::NetworkDisconnect,
                SystemAlertKind::DeviceOffline,
                SystemAlertKind::VersionMismatch,
            ]
        );
    }

    #[test]
    fn network_fires_on_no_internet_even_with_links_up() {
        let mut state = SystemAlertState::default();
        let mut snap = healthy();
        snap.has_internet = false;
        let events = state.evaluate(&snap, &SystemAlertConfig::default());
        assert_eq!(kinds(&events), vec![SystemAlertKind::NetworkDisconnect]);
        assert!(events[0].message.contains("no internet"));
    }

    #[test]
    fn unknown_values_neither_fire_nor_stay_latched() {
        let mut state = SystemAlertState::default();
        let cfg = SystemAlertConfig::default();

        let mut snap = healthy();
        snap.battery_pct = Some(5.0);
        assert_eq!(state.evaluate(&snap, &cfg).len(), 1);

        // Battery reading disappears → re-arms.
        snap.battery_pct = None;
        assert!(state.evaluate(&snap, &cfg).is_empty());
        snap.battery_pct = Some(5.0);
        assert_eq!(
            state.evaluate(&snap, &cfg).len(),
            1,
            "fires again after unknown"
        );
    }

    #[test]
    fn version_check_off_by_default_and_needs_both_sides() {
        let mut state = SystemAlertState::default();
        let mut snap = healthy();
        snap.version = Some("0.0.1".into());

        // Disabled by default even with expected_version set.
        let mut cfg = SystemAlertConfig {
            expected_version: Some("9.9.9".into()),
            ..SystemAlertConfig::default()
        };
        assert!(state.evaluate(&snap, &cfg).is_empty());

        cfg.version_enabled = true;
        assert_eq!(
            kinds(&state.evaluate(&snap, &cfg)),
            vec![SystemAlertKind::VersionMismatch]
        );

        // No expected version → nothing, and the debounce re-arms.
        cfg.expected_version = None;
        assert!(state.evaluate(&snap, &cfg).is_empty());
    }

    #[test]
    fn disabled_kinds_never_fire() {
        let cfg = SystemAlertConfig {
            battery_enabled: false,
            storage_enabled: false,
            portal_storage_enabled: false,
            network_enabled: false,
            offline_enabled: false,
            version_enabled: false,
            ..SystemAlertConfig::default()
        };
        let snap = SystemSnapshot {
            battery_pct: Some(1.0),
            storage_used_pct: Some(99.0),
            portal_disk_pct: Some(99.0),
            network_down: true,
            has_internet: false,
            online: false,
            version: Some("x".into()),
        };
        let mut state = SystemAlertState::default();
        assert!(state.evaluate(&snap, &cfg).is_empty());
    }

    #[test]
    fn subject_and_html() {
        let event = SystemAlertEvent::new(
            SystemAlertKind::BatteryLow,
            "Battery level is 5.0% (threshold 20%)".into(),
            "battery_low",
            vec![("pct", "5.0".into()), ("threshold", "20".into())],
        );
        assert_eq!(
            event.email_subject("fiber-01", "en"),
            "[SYSTEM ALERT] fiber-01 - Battery Low"
        );
        assert_eq!(
            event.email_subject("fiber-01", "cs"),
            "[PORUCHA] fiber-01 - Nízký stav baterie"
        );
        let cs = event.email_body("fiber-01", "cs", None).to_html();
        assert!(cs.contains("Baterie je na 5.0 % (mez 20 %)."), "{cs}");
        assert!(event
            .email_body("fiber-01", "en", None)
            .to_html()
            .contains("Battery level is 5.0% (threshold 20%)"));

        // UX-027: a fault links to its Hub's page, in both parts, and only
        // with a `public_url`.
        let body = event.email_body("proximos-ce3d59f8", "cs", Some("https://p.local"));
        let url = "https://p.local/hubs/proximos-ce3d59f8";
        assert!(body.to_html().contains(&format!("href=\"{url}\"")));
        let text = body.to_text();
        assert!(
            text.contains(&format!("Otevřít Hub v portálu: {url}\n")),
            "{text}"
        );
        assert!(text.contains("Zpráva: Baterie je na 5.0 % (mez 20 %).\n"));
        let none = event.email_body("proximos-ce3d59f8", "cs", None);
        assert!(!none.to_html().contains("http") && !none.to_text().contains("http"));

        // Quirk #8 fix: MQTT-sourced strings are escaped. An unknown key falls
        // back to the English message, escaped too.
        let hostile = SystemAlertEvent::new(
            SystemAlertKind::DeviceOffline,
            "<img src=x onerror=alert(1)>".into(),
            "no_such_key",
            Vec::new(),
        );
        let html = hostile.email_body("<b>host</b>", "en", None).to_html();
        assert!(!html.contains("<img"));
        assert!(!html.contains("<b>host</b>"));
        assert!(html.contains("&lt;b&gt;host&lt;/b&gt;"));
    }
}
