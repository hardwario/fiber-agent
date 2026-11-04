//! The shared "Channels" card ("Čidla") on the Overview of a Node, a Beacon
//! and a wired probe (plan/portal/ui_components.md §2, amended 2026-10-15
//! §2.5 and §2.6): every channel the device has reported, where it comes
//! from, its last value and when it was received. No chart and no
//! statistics; those live in Trends ("Open in Trends").
//!
//! A manager switches Payload channels on and off from the card ("Set
//! channels", `ui.edit_set`); Save posts the existing per-sensor routes
//! (`POST /nodes/{key}/sensors/{field}`, `POST /beacons/{mac}/sensors/{field}`).
//!
//! This module only builds the card's context; the pages own their routes.

use std::collections::BTreeMap;

use portal_storage::EyeField;
use serde::Serialize;

use crate::http::AppState;
use crate::render::{NodeDetailCtx, ThresholdEntryCtx};

use super::tr;

/// A channel older than this is drawn muted ("stale").
pub(crate) const STALE_AFTER_S: i64 = 3600;

/// Where a channel's value comes from — the Source badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Origin {
    /// What the device measures (node_field_readings / counters; a Beacon's
    /// temperature and humidity). The only rows a manager can switch.
    Payload,
    /// The device's own health: battery, DeviceInfo.
    Diagnostics,
    /// The link: RSSI, SNR, gateways, frame counter.
    Radio,
    /// Computed by the Hub: probe averages, the home Hub.
    System,
}

/// One row of the card.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ChannelRow {
    /// Unique within the card (the field name, or `rssi`, `hub`, …).
    pub id: String,
    /// Already translated.
    pub label: String,
    pub unit: String,
    pub source: Origin,
    /// The displayed value, `None` = "—".
    pub last: Option<String>,
    /// The sort key of Last: the number where there is one, else the text.
    pub sort_last: String,
    /// Unix seconds of the last receipt, `None` = unknown ("—").
    pub ts: Option<i64>,
    /// `3 min ago` etc., `None` with `ts`.
    pub received: Option<String>,
    /// RFC 3339 of `ts`, for the cell's title.
    pub received_at: Option<String>,
    pub stale: bool,
    /// `false` = a switched-off sensor: it stays listed, muted, "off".
    pub enabled: bool,
    /// The switch route, set only on a row a manager may switch.
    pub action: Option<String>,
    /// Switching it off drafts a new alarm table (the Node holds its band).
    pub has_band: bool,
    /// The row's limits (ui_components.md §2.8): the mini bar and, for a
    /// manager in edit mode, the inline limits editor. `None` = a row that
    /// takes no limits (the column stays empty).
    pub limit: Option<LimitRef>,
}

/// Where a row's limits live and what is stored; the template resolves the
/// band it opens on with `limits_band(scope, field, stored)`, as the limits
/// editor does.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct LimitRef {
    /// The device's own threshold route (the limits editor's `le_post`).
    pub post: String,
    /// "node" | "beacon" | "probe"
    pub kind: &'static str,
    /// "node" | "eye" | "hub" — the defaults' scope.
    pub scope: &'static str,
    pub field: String,
    /// The "Limits enabled" switch exists (not on a wired probe).
    pub toggle: bool,
    /// The stored band; `None` = nothing configured ("—").
    pub stored: Option<StoredBand>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StoredBand {
    pub critical_low: Option<f64>,
    pub warning_low: Option<f64>,
    pub warning_high: Option<f64>,
    pub critical_high: Option<f64>,
    pub enabled: bool,
    pub hysteresis: Option<f64>,
}

impl From<&ThresholdEntryCtx> for StoredBand {
    fn from(t: &ThresholdEntryCtx) -> Self {
        Self {
            critical_low: t.critical_low,
            warning_low: t.warning_low,
            warning_high: t.warning_high,
            critical_high: t.critical_high,
            enabled: t.enabled,
            hysteresis: t.hysteresis,
        }
    }
}

/// `partials/channels_card.html`'s context (`ch`).
#[derive(Debug, Serialize)]
pub(crate) struct ChannelsCtx {
    pub card_id: String,
    pub rows: Vec<ChannelRow>,
    /// "Set channels" is drawn: a manager, and a row it can switch.
    pub can_set: bool,
    /// The confirm dialog says a draft follows (a Node card).
    pub node: bool,
    pub trends_href: Option<String>,
}

fn decimals(unit: &str) -> usize {
    match unit {
        "%" => 0,
        "V" => 2,
        _ => 1,
    }
}

fn fmt_prec(v: f64, prec: usize) -> String {
    let s = format!("{v:.prec$}");
    // A rounded −0.04 prints as "-0.0".
    if s.trim_start_matches('-')
        .chars()
        .all(|c| c == '0' || c == '.')
    {
        s.trim_start_matches('-').to_string()
    } else {
        s
    }
}

/// A row builder; `now` is passed in so tests are deterministic.
struct Rows {
    now: i64,
    out: Vec<ChannelRow>,
}

impl Rows {
    fn new(now: i64) -> Self {
        Self {
            now,
            out: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn num(
        &mut self,
        id: &str,
        label: String,
        unit: &str,
        source: Origin,
        value: Option<f64>,
        prec: usize,
        ts: Option<i64>,
    ) -> &mut ChannelRow {
        let last = value.map(|v| fmt_prec(v, prec));
        let sort = value.map(|v| v.to_string()).unwrap_or_default();
        self.push(id, label, unit, source, last, sort, ts)
    }

    fn text(
        &mut self,
        id: &str,
        label: String,
        source: Origin,
        value: Option<String>,
        ts: Option<i64>,
    ) -> &mut ChannelRow {
        let sort = value.clone().unwrap_or_default();
        self.push(id, label, "", source, value, sort, ts)
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        id: &str,
        label: String,
        unit: &str,
        source: Origin,
        last: Option<String>,
        sort_last: String,
        ts: Option<i64>,
    ) -> &mut ChannelRow {
        self.out.push(ChannelRow {
            id: id.to_string(),
            label,
            unit: unit.to_string(),
            source,
            last,
            sort_last,
            ts,
            received: ts.map(crate::monitoring::relative_time),
            received_at: ts.and_then(crate::render::unix_to_rfc3339),
            stale: ts.is_some_and(|t| self.now - t >= STALE_AFTER_S),
            enabled: true,
            action: None,
            has_band: false,
            limit: None,
        });
        self.out.last_mut().expect("just pushed")
    }
}

fn rfc3339_unix(s: Option<&str>) -> Option<i64> {
    time::OffsetDateTime::parse(s?, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp())
}

/// The read-only times a Node's rows need: each field's newest stored
/// time and the newest raw uplink. A failed read is "unknown" ("—").
pub(crate) async fn node_times(
    state: &AppState,
    hostname: &str,
    node_key: &str,
) -> (BTreeMap<String, i64>, Option<i64>) {
    let (h, n) = (hostname.to_string(), node_key.to_string());
    let read = state
        .db
        .run(move |s| {
            Ok((
                s.latest_node_field_times(&h, &n)?,
                // NOTE: only the Radio rows read it (SHOW_NON_PAYLOAD).
                if SHOW_NON_PAYLOAD {
                    s.latest_raw_message_at(&h, &n)?
                } else {
                    None
                },
            ))
        })
        .await;
    match read {
        Ok((fields, raw)) => {
            let mut times = BTreeMap::new();
            for (name, ts) in fields {
                let e = times.entry(name).or_insert(ts);
                *e = (*e).max(ts);
            }
            (times, raw)
        }
        Err(err) => {
            tracing::warn!(%err, node_key, "channels: the receive times are unreadable");
            (BTreeMap::new(), None)
        }
    }
}

/// A Node's card: every active field (Payload; the battery is
/// Diagnostics), DeviceInfo, the radio link and its Hub.
#[allow(clippy::too_many_arguments)]
pub(crate) fn node_card(
    card_id: String,
    hub: &str,
    n: &NodeDetailCtx,
    thresholds: &BTreeMap<String, ThresholdEntryCtx>,
    times: &BTreeMap<String, i64>,
    raw_at: Option<i64>,
    can_edit: bool,
    now: i64,
) -> ChannelsCtx {
    let lang = crate::i18n::current_lang();
    let last_seen = rfc3339_unix(n.last_seen.as_deref());
    let radio_ts = raw_at.or(last_seen);
    let mut rows = Rows::new(now);
    let mut tokens = Vec::new();
    let mut battery_field = false;
    for f in &n.fields {
        let Some(def) = fiber_protocol::lorawan_field(&f.name) else {
            continue;
        };
        let battery = def.group == fiber_protocol::LorawanFieldGroup::Battery;
        battery_field |= battery;
        let source = if battery {
            Origin::Diagnostics
        } else {
            Origin::Payload
        };
        let prec = if f.count { 0 } else { decimals(&f.unit) };
        let label = crate::i18n::field_label(lang, def.name, &f.label);
        let row = rows.num(
            def.name,
            label,
            &f.unit,
            source,
            f.value,
            prec,
            times.get(def.name).copied(),
        );
        row.enabled = f.enabled;
        if source == Origin::Payload && can_edit {
            row.action = Some(format!("/nodes/{}/sensors/{}", n.node_key, def.name));
            row.has_band = thresholds.get(def.name).is_some_and(|t| {
                t.enabled && t.critical_low.is_some() && t.critical_high.is_some()
            });
        }
        if f.thresholdable {
            row.limit = Some(LimitRef {
                post: format!("/nodes/{}/thresholds/{}", n.node_key, def.name),
                kind: "node",
                scope: "node",
                field: def.name.to_string(),
                toggle: true,
                stored: thresholds.get(def.name).map(StoredBand::from),
            });
        }
        if f.enabled && f.chartable && !f.count {
            tokens.push(format!("node:{}:{}", n.node_key, def.name));
        }
    }

    let info_ts = rfc3339_unix(n.info_received_at.as_deref());
    if !battery_field {
        if let Some(b) = n.info_view.battery.clone() {
            let value = b.trim_end_matches(" V").trim().to_string();
            rows.text(
                "info_battery",
                crate::i18n::field_label(lang, "battery", "Battery"),
                Origin::Diagnostics,
                Some(value),
                info_ts,
            )
            .unit = "V".into();
        }
    }
    if let Some(up) = n.info_view.uptime.clone() {
        rows.text(
            "uptime",
            tr("monitoring.device_info.uptime"),
            Origin::Diagnostics,
            Some(up),
            info_ts,
        );
    }

    rows.num(
        "rssi",
        tr("monitoring.list.column_rssi"),
        "dBm",
        Origin::Radio,
        n.rssi.map(|v| v as f64),
        0,
        n.rssi.and(radio_ts),
    );
    rows.num(
        "snr",
        tr("monitoring.list.column_snr"),
        "dB",
        Origin::Radio,
        n.snr,
        1,
        n.snr.and(radio_ts),
    );
    let gateways: Vec<String> = n
        .gateways
        .iter()
        .filter_map(|g| g.gateway_id.clone())
        .collect();
    let gw = (!gateways.is_empty()).then(|| gateways.join(", "));
    let gw_ts = gw.as_ref().and(radio_ts);
    rows.text(
        "gateways",
        tr("monitoring.channels.gateways"),
        Origin::Radio,
        gw,
        gw_ts,
    );
    rows.num(
        "frame_counter",
        tr("monitoring.channels.frame_counter"),
        "",
        Origin::Radio,
        n.uplink_counter.map(|v| v as f64),
        0,
        n.uplink_counter.and(radio_ts),
    );
    rows.text(
        "hub",
        tr("monitoring.device_info.hub"),
        Origin::System,
        Some(hub.to_string()),
        last_seen,
    );

    finish(card_id, rows.out, can_edit, true, &tokens)
}

/// A Beacon's card: temperature / humidity (Payload, switchable), battery
/// (Diagnostics), RSSI (Radio) and the home Hub (System). The store keeps
/// one receive time per Beacon, so every row carries it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn beacon_card(
    card_id: String,
    mac: &str,
    fields: &[crate::render::BeaconFieldCtx],
    thresholds: Option<&BTreeMap<String, ThresholdEntryCtx>>,
    battery_mv: Option<i64>,
    rssi: Option<i64>,
    home_hub: Option<&str>,
    last_seen: Option<i64>,
    can_edit: bool,
    now: i64,
) -> ChannelsCtx {
    let lang = crate::i18n::current_lang();
    let mut rows = Rows::new(now);
    let mut tokens = Vec::new();
    for f in fields {
        let Some(column) = EyeField::from_field_name(&f.name) else {
            continue;
        };
        let label = crate::i18n::field_label(lang, &f.name, &f.label);
        let row = rows.num(
            &f.name,
            label,
            &f.unit,
            Origin::Payload,
            f.value,
            decimals(&f.unit),
            f.value.and(last_seen),
        );
        row.enabled = f.enabled;
        if can_edit {
            row.action = Some(format!("/beacons/{mac}/sensors/{}", f.name));
        }
        row.limit = Some(LimitRef {
            post: format!("/beacons/{mac}/thresholds/{}", f.name),
            kind: "beacon",
            scope: "eye",
            field: f.name.clone(),
            toggle: true,
            stored: thresholds
                .and_then(|t| t.get(&f.name))
                .map(StoredBand::from),
        });
        if f.enabled {
            let name = match column {
                EyeField::Temperature => "temperature",
                EyeField::Humidity => "humidity",
            };
            tokens.push(format!("eye:{mac}:{name}"));
        }
    }
    rows.num(
        "battery",
        crate::i18n::field_label(lang, "battery", "Battery"),
        "V",
        Origin::Diagnostics,
        battery_mv.map(|mv| mv as f64 / 1000.0),
        2,
        battery_mv.and(last_seen),
    );
    rows.num(
        "rssi",
        tr("monitoring.list.column_rssi"),
        "dBm",
        Origin::Radio,
        rssi.map(|v| v as f64),
        0,
        rssi.and(last_seen),
    );
    if let Some(hub) = home_hub {
        rows.text(
            "home_hub",
            tr("monitoring.device_info.home_hub"),
            Origin::System,
            Some(hub.to_string()),
            last_seen,
        );
    }
    finish(card_id, rows.out, can_edit, false, &tokens)
}

/// A wired probe's card: the Hub's average of the line is the probe's
/// reading (Payload); minimum and maximum are Hub statistics (System).
/// Nothing to switch.
pub(crate) fn probe_card(
    card_id: String,
    hub: &str,
    line: u8,
    s: &crate::render::SensorCardCtx,
    now: i64,
) -> ChannelsCtx {
    let ts = rfc3339_unix(s.updated_at.as_deref());
    let mut rows = Rows::new(now);
    let unit = "\u{b0}C";
    for (id, key, v) in [
        ("avg", "monitoring.channels.probe_avg", s.avg_c),
        ("min", "monitoring.channels.probe_min", s.min_c),
        ("max", "monitoring.channels.probe_max", s.max_c),
    ] {
        let src = if id == "avg" {
            Origin::Payload
        } else {
            Origin::System
        };
        let row = rows.num(id, tr(key), unit, src, v, 1, v.and(ts));
        if id == "avg" {
            // The line's band, with its "Mez zapnuta" switch (#167).
            row.limit = Some(LimitRef {
                post: format!("/hubs/{hub}/sensors/{line}/threshold"),
                kind: "probe",
                scope: "hub",
                field: "temperature".into(),
                toggle: true,
                stored: s.thresholds.as_ref().map(|t| StoredBand {
                    critical_low: t.critical_low,
                    warning_low: t.warning_low,
                    warning_high: t.warning_high,
                    critical_high: t.critical_high,
                    enabled: t.enabled,
                    hysteresis: t.hysteresis,
                }),
            });
        }
    }
    let token = format!("line:{}:{line}", hub.to_lowercase());
    finish(card_id, rows.out, s.can_edit, false, &[token])
}

/// NOTE: kept for possible return: non-payload channels (Diagnostics/Radio/
/// System), see plan/portal/ui_components.md §2.7. Hynek 2026-10-15: the
/// card lists Payload rows only. The builders above still map every source;
/// flip this to bring the other rows (and the raw-message query in
/// [`node_times`]) back.
pub(crate) const SHOW_NON_PAYLOAD: bool = false;

fn finish(
    card_id: String,
    mut rows: Vec<ChannelRow>,
    can_edit: bool,
    node: bool,
    tokens: &[String],
) -> ChannelsCtx {
    if !SHOW_NON_PAYLOAD {
        rows.retain(|r| r.source == Origin::Payload);
    }
    ChannelsCtx {
        card_id,
        can_set: can_edit && rows.iter().any(|r| r.action.is_some() || r.limit.is_some()),
        node,
        trends_href: (!tokens.is_empty()).then(|| format!("/trends?s={}", tokens.join(","))),
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(
        name: &str,
        label: &str,
        unit: &str,
        value: f64,
        enabled: bool,
    ) -> crate::render::BeaconFieldCtx {
        crate::render::BeaconFieldCtx {
            name: name.into(),
            label: label.into(),
            unit: unit.into(),
            value: Some(value),
            alarm_state: "Normal".into(),
            enabled,
        }
    }

    #[test]
    fn a_beacon_card_maps_sources_and_switches_only_payload() {
        let now = 10_000;
        let ch = beacon_card(
            "c".into(),
            "AA:BB",
            &[
                field("temperature", "Temperature", "°C", -0.04, true),
                field("humidity", "Humidity", "%", 55.0, false),
            ],
            None,
            Some(3010),
            Some(-80),
            Some("hub-a"),
            Some(now - 7200),
            true,
            now,
        );
        let by = |id: &str| ch.rows.iter().find(|r| r.id == id).expect(id);
        assert_eq!(by("temperature").source, Origin::Payload);
        assert_eq!(by("temperature").last.as_deref(), Some("0.0"));
        assert!(by("temperature").stale, "two hours old");
        assert!(by("temperature").action.is_some());
        assert!(
            !by("humidity").enabled,
            "a switched-off channel stays listed"
        );
        // Payload only while SHOW_NON_PAYLOAD is off.
        assert!(
            ch.rows.iter().all(|r| r.source == Origin::Payload),
            "battery, rssi and home hub rows are held back"
        );
        assert!(ch.can_set);
        assert_eq!(
            ch.trends_href.as_deref(),
            Some("/trends?s=eye:AA:BB:temperature")
        );
    }

    #[test]
    fn a_viewer_gets_no_switches_and_an_unknown_time_is_none() {
        let ch = beacon_card(
            "c".into(),
            "AA:BB",
            &[field("temperature", "T", "°C", 1.0, true)],
            None,
            None,
            None,
            None,
            None,
            false,
            0,
        );
        assert!(!ch.can_set);
        assert!(ch.rows.iter().all(|r| r.action.is_none()));
        assert!(ch
            .rows
            .iter()
            .all(|r| r.ts.is_none() && r.received.is_none()));
        assert!(ch.rows.iter().all(|r| !r.stale));
    }
}
