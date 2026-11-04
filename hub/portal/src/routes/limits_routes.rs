//! `GET /limits` — every channel of every sensor on every Hub in one table,
//! with the limit it carries and whether that limit is in force
//! (plan/portal/limits.cs.md).
//!
//! The page is a read over what already exists, not a fourth place limits
//! live: a wired line's band is the Hub's `config/state` echo, a Node's is
//! the firmware echo merged with its `field_thresholds` row, a Beacon's is
//! its `field_thresholds` row. A Node's draft in `pending_config` is what
//! makes a row "waiting". Setting limits is the one limits editor
//! (`partials/limits_editor.html`, limits.cs.md §9.4), which writes through
//! each device's own threshold route.

use std::collections::{BTreeMap, HashMap};

use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use minijinja::context;
use portal_storage::{
    FieldThreshold, PendingConfigRow, PENDING_STATE_ERROR, PENDING_STATE_SENDING,
    PENDING_STATE_UNDELIVERED, THRESHOLD_SCOPE_EYE, THRESHOLD_SCOPE_NODE,
};
use serde::Serialize;

use crate::auth::CurrentUser;
use crate::http::AppState;
use crate::render;
use crate::state::models::DeviceState;

use super::tr;

/// The Node's battery voltage (`fiber_protocol` lorawan registry).
const BATTERY_FIELD: &str = "voltage";

pub fn router() -> Router<AppState> {
    Router::new().route("/limits", get(limits_page))
}

/// One channel with its limit, as the table row renders it (limits.cs.md §9.2).
#[derive(Debug, Clone, Serialize)]
struct LimitRowCtx {
    kind: &'static str,
    hub: String,
    /// Unique on the page: the device and the field, `node:{key}:{field}` …
    id: String,
    /// The registry field name, the quantity this row measures.
    field: String,
    sensor: String,
    href: String,
    field_label: String,
    /// The displayed unit, and with the field what the editor may mix
    /// (§9.3: one quantity and unit per selection).
    unit: String,
    value: Option<f64>,
    reading: Option<String>,
    /// "critical" | "warning" | "normal" — the value class of the reading.
    oor: &'static str,
    /// critical_low, warning_low, warning_high, critical_high, displayed.
    bounds: [Option<String>; 4],
    /// Per bound: shown from a Node's draft, not yet confirmed by the Node (decision #41
    /// exception: a Node's limit holds only once the Node confirmed it).
    /// `Some(the Node's current value or "—")` where the bound is shown from the draft.
    pending: [Option<String>; 4],
    /// "ok" | "unset" | "off" | "draft" | "sending" | "error" | "undelivered" | "offline"
    state: &'static str,
    /// Who drafted it, or why it was refused or not delivered.
    note: Option<String>,
    /// config_sync.md §13 Q20e: the "not synchronised with the Node" badge text, and where
    /// it links (the Node detail's banner).
    unsynced: Option<String>,
    unsynced_href: Option<String>,
    location: Option<String>,
    connected: bool,
    /// The device's own POST for this field's limit, and the defaults' scope.
    post: String,
    scope: &'static str,
    /// What is stored, in the displayed unit, for `limits_band`; none = no band yet.
    stored: Option<StoredCtx>,
    /// False for a sensor switched off: its limit is not edited here.
    editable: bool,
}

#[derive(Debug, Clone, Serialize)]
struct StoredCtx {
    critical_low: Option<f64>,
    warning_low: Option<f64>,
    warning_high: Option<f64>,
    critical_high: Option<f64>,
    enabled: bool,
    hysteresis: Option<f64>,
}

impl StoredCtx {
    fn new(b: Bounds, enabled: bool, hysteresis: Option<f64>) -> Self {
        Self {
            critical_low: b.0,
            warning_low: b.1,
            warning_high: b.2,
            critical_high: b.3,
            enabled,
            hysteresis,
        }
    }
}

type Bounds = (Option<f64>, Option<f64>, Option<f64>, Option<f64>);

fn fmt(v: f64) -> String {
    format!("{v:.1}")
}

fn shown(b: Bounds, f: impl Fn(f64) -> f64) -> [Option<String>; 4] {
    [b.0, b.1, b.2, b.3].map(|v| v.map(|v| fmt(f(v))))
}

fn bounded(b: Bounds) -> bool {
    b.0.is_some() || b.1.is_some() || b.2.is_some() || b.3.is_some()
}

fn class_of(firmware_state: Option<&String>) -> &'static str {
    match firmware_state.map(|s| s.to_ascii_uppercase()).as_deref() {
        Some("CRITICAL") => "critical",
        Some("WARNING") => "warning",
        _ => "normal",
    }
}

fn with_unit(v: f64, unit: Option<&str>) -> String {
    match unit {
        Some(u) if !u.is_empty() => format!("{} {u}", fmt(v)),
        _ => fmt(v),
    }
}

/// A limit's state from its bounds and switch alone — what a wired line or
/// a Beacon can be in, since both take effect when the Hub answers.
fn settled(b: Bounds, enabled: bool) -> &'static str {
    match (bounded(b), enabled) {
        (false, _) => "unset",
        (true, false) => "off",
        (true, true) => "ok",
    }
}

/// A Node field's draft decides its state when it has one: refused beats
/// undelivered beats sending beats merely drafted, and an offline Hub is why
/// nothing moves. An undelivered bound is in force on the Hub, only the Node
/// lacks it, so it is not an error.
fn drafted(rows: &[&PendingConfigRow], online: bool) -> Option<(&'static str, Option<String>)> {
    let first = rows.first()?;
    if let Some(r) = rows.iter().find(|r| r.state == PENDING_STATE_ERROR) {
        return Some(("error", r.last_error.clone()));
    }
    if let Some(r) = rows.iter().find(|r| r.state == PENDING_STATE_UNDELIVERED) {
        return Some(("undelivered", r.last_error.clone()));
    }
    if !online {
        return Some(("offline", None));
    }
    let state = if rows.iter().any(|r| r.state == PENDING_STATE_SENDING) {
        "sending"
    } else {
        "draft"
    };
    Some((
        state,
        Some(tr("monitoring.limits.drafted_by").replace("{user}", &first.author)),
    ))
}

/// The bound a Node still holds while `r` drafts a new one, as displayed. Until the Hub takes
/// the draft that is the stored bound; once it has, `field_thresholds` carries the new one
/// and the old lives in the row's `previous_value`.
fn held_by_node(
    r: &PendingConfigRow,
    stored: Option<String>,
    display: impl Fn(f64) -> f64,
) -> String {
    match r.previous_value.as_deref() {
        Some(prev) => serde_json::from_str::<Option<f64>>(prev)
            .ok()
            .flatten()
            .map(|v| fmt(display(v))),
        None => stored,
    }
    .unwrap_or_else(|| "—".to_string())
}

struct HubRows {
    node: HashMap<String, Vec<FieldThreshold>>,
    eye: HashMap<String, Vec<FieldThreshold>>,
    pending: HashMap<String, Vec<PendingConfigRow>>,
    delivery: HashMap<String, Vec<portal_storage::NodeThresholdDelivery>>,
}

async fn hub_rows(state: &AppState, hostname: &str) -> HubRows {
    let h = hostname.to_string();
    let read = state.db.run(move |s| {
        Ok(HubRows {
            node: s.field_thresholds_by_entity(&h, THRESHOLD_SCOPE_NODE)?,
            eye: s.field_thresholds_by_entity(&h, THRESHOLD_SCOPE_EYE)?,
            pending: s.list_pending_by_node(&h)?,
            delivery: s.list_node_delivery_by_hub(&h)?,
        })
    });
    read.await.unwrap_or_else(|err| {
        tracing::warn!(%err, hostname, "limits: stored limits unreadable");
        HubRows {
            node: HashMap::new(),
            eye: HashMap::new(),
            pending: HashMap::new(),
            delivery: HashMap::new(),
        }
    })
}

/// Every channel of one Hub, in the order the device lists show them:
/// wired lines, then Nodes, then Beacons.
fn device_rows(device: &DeviceState, stored: &HubRows) -> Vec<LimitRowCtx> {
    let host = device.hostname.as_str();
    let mut out = Vec::new();

    for s in &device.sensors {
        // A line the Hub has never reported on is not a sensor yet.
        if s.updated_at.is_none() && s.thresholds.is_none() {
            continue;
        }
        let b = s
            .thresholds
            .as_ref()
            .map(|t| {
                (
                    t.critical_low,
                    t.warning_low,
                    t.warning_high,
                    t.critical_high,
                )
            })
            .unwrap_or_default();
        // The band's switch (#167): off keeps the bounds and shows "vypnuto".
        let probe_on = s.thresholds.as_ref().is_none_or(|t| t.is_enabled());
        out.push(LimitRowCtx {
            kind: "probe",
            hub: host.to_string(),
            id: format!("probe:{host}:{}", s.line),
            field: "temperature".to_string(),
            sensor: s.name.clone().unwrap_or_else(|| {
                tr("monitoring.shared.line_fallback").replace("{n}", &(s.line + 1).to_string())
            }),
            href: crate::monitoring::probe_href(host, s.line),
            field_label: tr("monitoring.beacon_card.temperature_label"),
            unit: "°C".to_string(),
            value: s.temperature_avg_celsius,
            reading: s.temperature_avg_celsius.map(|v| with_unit(v, Some("°C"))),
            oor: class_of(Some(&s.dominant_alarm_state)),
            bounds: shown(b, |v| v),
            pending: [None, None, None, None],
            state: settled(b, probe_on),
            note: None,
            unsynced: None,
            unsynced_href: None,
            location: s.location.clone(),
            connected: s.connected,
            post: format!("/hubs/{host}/sensors/{}/threshold", s.line),
            scope: "hub",
            stored: s
                .thresholds
                .as_ref()
                .map(|t| StoredCtx::new(b, t.is_enabled(), t.hysteresis)),
            editable: true,
        });
    }

    for (key, n) in &device.nodes {
        let limits = render::merge_node_thresholds(
            n.field_thresholds.as_deref(),
            stored.node.get(key).map(Vec::as_slice).unwrap_or_default(),
        );
        let draft = stored.pending.get(key);
        let node_layout = {
            let active = render::active_node_fields(device.node_configs.get(key), n);
            crate::radio::alarm::slot_layout(active.iter().map(|f| f.name.as_str()))
        };
        let connected = !matches!(n.alarm_state.as_str(), "Disconnected" | "NeverConnected");
        for f in fiber_protocol::lorawan_registry()
            .iter()
            .filter(|f| f.thresholdable)
        {
            let reading = n.fields.get(f.name).map(|&v| f.to_display(v));
            let limit = limits.get(f.name);
            if reading.is_none() && limit.is_none() {
                continue;
            }
            // Battery is a fault (glossary: Porucha), and Monitoring leaves it
            // out of its default fields for the same reason. Its limit still
            // alarms, so a set one is shown; an unset one is not a gap.
            let set = limit.is_some_and(|t| {
                bounded((
                    t.critical_low,
                    t.warning_low,
                    t.warning_high,
                    t.critical_high,
                ))
            });
            if f.name == BATTERY_FIELD && !set {
                continue;
            }
            let b = limit
                .map(|t| {
                    (
                        t.critical_low,
                        t.warning_low,
                        t.warning_high,
                        t.critical_high,
                    )
                })
                .unwrap_or_default();
            let prefix = format!("threshold.{}.", f.name);
            let rows: Vec<&PendingConfigRow> = draft
                .into_iter()
                .flatten()
                .filter(|r| r.param_key.starts_with(&prefix))
                .collect();
            let (state, note) = drafted(&rows, device.online)
                .unwrap_or((settled(b, limit.is_none_or(|t| t.enabled)), None));
            // The draft's bounds, shown in place of the Node's until it confirms them.
            let mut pending: [Option<String>; 4] = [None, None, None, None];
            let mut bounds = shown(b, |v| f.to_display(v));
            for (i, bound) in crate::pending::THRESHOLD_BOUNDS.iter().enumerate() {
                let key = crate::pending::threshold_param_key(f.name, bound);
                if let Some(r) = rows.iter().find(|r| r.param_key == key) {
                    let v = r
                        .desired_value
                        .as_deref()
                        .and_then(|v| serde_json::from_str::<Option<f64>>(v).ok())
                        .flatten();
                    let current = held_by_node(r, bounds[i].clone(), |v| f.to_display(v));
                    bounds[i] = v.map(|v| fmt(f.to_display(v)));
                    pending[i] = Some(current);
                }
            }
            // config_sync.md §13 Q20e: the Node confirmed another critical band, nothing pending.
            let unsynced = if draft.is_none_or(|d| d.is_empty())
                && node_layout.contains(&f.name.to_string())
            {
                limit.and_then(|t| {
                    render::node_unsynced(
                        f.name,
                        &t.clone().displayed(f),
                        stored
                            .delivery
                            .get(key)
                            .and_then(|d| d.iter().find(|d| d.field == f.name)),
                    )
                })
            } else {
                None
            };
            let unsynced = unsynced.map(|u| {
                let band = if u.off {
                    format!(
                        "{} ({})",
                        u.band,
                        tr("monitoring.threshold_table.off_label")
                    )
                } else {
                    u.band
                };
                tr("monitoring.threshold_table.node_unsynced_badge").replace("{band}", &band)
            });
            let unsynced_href = unsynced
                .as_ref()
                .map(|_| format!("/nodes/{key}/overview#node-unsynced-{key}"));
            let disp = |v: Option<f64>| v.map(|v| f.to_display(v));
            let shown_b = (disp(b.0), disp(b.1), disp(b.2), disp(b.3));
            out.push(LimitRowCtx {
                unsynced,
                unsynced_href,
                kind: "node",
                hub: host.to_string(),
                id: format!("node:{key}:{}", f.name),
                field: f.name.to_string(),
                sensor: n.name.clone().unwrap_or_else(|| key.clone()),
                href: format!("/nodes/{key}/overview"),
                field_label: crate::i18n::field_label(crate::i18n::current_lang(), f.name, f.label),
                unit: f.display_unit().unwrap_or("").to_string(),
                value: reading,
                reading: reading.map(|v| with_unit(v, f.display_unit())),
                oor: class_of(n.field_alarm_states.get(f.name)),
                bounds,
                pending,
                state,
                note,
                location: n.location.clone(),
                connected,
                post: format!("/nodes/{key}/thresholds/{}", f.name),
                scope: "node",
                stored: limit.map(|t| {
                    StoredCtx::new(
                        shown_b,
                        t.enabled,
                        t.hysteresis.map(|h| f.to_display(h) - f.to_display(0.0)),
                    )
                }),
                editable: f.name != BATTERY_FIELD && !n.disabled_fields.contains_key(f.name),
            });
        }
    }

    for (mac, b) in &device.ble_sensors {
        // A discovered tag is not a registered Beacon; it has no limits.
        if b.discovered == Some(true) {
            continue;
        }
        let stored_rows = stored.eye.get(mac);
        for f in fiber_protocol::eye_registry()
            .iter()
            .filter(|f| f.thresholdable)
        {
            let reading = match f.name {
                "temperature" => b.temperature,
                "humidity" => b.humidity.map(|h| h as f64),
                _ => None,
            };
            let row = stored_rows.and_then(|rows| rows.iter().find(|r| r.field == f.name));
            if reading.is_none() && row.is_none() {
                continue;
            }
            let bounds = row
                .map(|r| {
                    (
                        r.critical_low,
                        r.warning_low,
                        r.warning_high,
                        r.critical_high,
                    )
                })
                .unwrap_or_default();
            out.push(LimitRowCtx {
                kind: "beacon",
                hub: host.to_string(),
                id: format!("beacon:{mac}:{}", f.name),
                field: f.name.to_string(),
                sensor: b.name.clone().unwrap_or_else(|| mac.clone()),
                href: format!("/beacons/{mac}/overview"),
                field_label: crate::i18n::field_label(crate::i18n::current_lang(), f.name, f.label),
                unit: f.unit.unwrap_or("").to_string(),
                value: reading,
                reading: reading.map(|v| with_unit(v, f.unit)),
                oor: class_of(b.field_alarm_states.get(f.name)),
                bounds: shown(bounds, |v| v),
                pending: [None, None, None, None],
                state: settled(bounds, row.is_none_or(|r| r.enabled)),
                note: None,
                unsynced: None,
                unsynced_href: None,
                location: b.location.clone(),
                connected: b.connected,
                post: format!("/beacons/{mac}/thresholds/{}", f.name),
                scope: "eye",
                stored: row.map(|r| StoredCtx::new(bounds, r.enabled, r.hysteresis)),
                editable: !b.disabled_fields.contains_key(f.name),
            });
        }
    }
    out
}

/// What the limits editor opens on for one field (limits.cs.md §9.4) — the one place the
/// starting values are decided, for /limits and for the device pages alike (the latter
/// through the `limits_band` template function, `i18n::register`).
///
/// A field with a stored band shows it as stored. One without shows the configured default
/// of its quantity (threshold_defaults.md D5) switched OFF: a default is a suggestion, and
/// one Save must not arm a threshold nobody asked for. A Node field the Node watches itself
/// opens with its critical sides preset to the open band when both are blank (P-3,
/// nodes_phase2.cs.md). The hysteresis is the field's own, else its quantity's default.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct EditorBand {
    pub critical_low: Option<f64>,
    pub warning_low: Option<f64>,
    pub warning_high: Option<f64>,
    pub critical_high: Option<f64>,
    pub enabled: bool,
    pub hysteresis: f64,
}

/// `scope` is "node", "eye" or "hub"; `stored` = (bounds, enabled, own hysteresis).
pub(crate) fn editor_band(
    scope: &str,
    field: &str,
    stored: Option<(Bounds, bool, Option<f64>)>,
) -> EditorBand {
    let scope_key = match scope {
        "node" => Some(THRESHOLD_SCOPE_NODE),
        "eye" => Some(THRESHOLD_SCOPE_EYE),
        "hub" => Some(portal_storage::THRESHOLD_SCOPE_HUB),
        _ => None,
    };
    let defaults = crate::threshold_defaults::configured();
    let default_hyst = scope_key
        .map(|k| crate::threshold_defaults::hysteresis(&defaults, k, field, None))
        .unwrap_or(0.0);
    let (b, enabled, own) = match stored {
        Some(s) => s,
        None => {
            let band = scope_key
                .filter(|k| *k != portal_storage::THRESHOLD_SCOPE_HUB)
                .and_then(|k| crate::threshold_defaults::default_band(&defaults, k, field))
                .map(|d| {
                    (
                        d.critical_low,
                        d.warning_low,
                        d.warning_high,
                        d.critical_high,
                    )
                })
                .unwrap_or_default();
            (band, false, None)
        }
    };
    let mut b = b;
    if scope == "node" && b.0.is_none() && b.3.is_none() {
        if let Some((low, high)) = crate::radio::alarm::open_band(field) {
            b.0 = Some(low);
            b.3 = Some(high);
        }
    }
    EditorBand {
        critical_low: b.0,
        warning_low: b.1,
        warning_high: b.2,
        critical_high: b.3,
        enabled,
        hysteresis: own.unwrap_or(default_hyst),
    }
}

/// The quantity filter's options in the bulk panel's order: temperature,
/// humidity, then the rest by label.
fn quantity_options(fields: BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = fields.into_iter().collect();
    out.sort_by_key(|(name, label)| {
        let rank = match name.as_str() {
            "temperature" => 0,
            "humidity" => 1,
            _ => 2,
        };
        (rank, label.clone())
    });
    out
}

async fn limits_page(current_user: CurrentUser, State(state): State<AppState>) -> Response {
    let snapshot = state.store.snapshot();
    let mut hubs: Vec<&DeviceState> = snapshot.devices.values().collect();
    hubs.sort_by(|a, b| a.hostname.cmp(&b.hostname));
    let mut rows = Vec::new();
    for device in &hubs {
        let stored = hub_rows(&state, &device.hostname).await;
        rows.extend(device_rows(device, &stored));
    }
    // The quantities on offer are the ones the fleet has, labelled as the
    // rows are; the filter itself is client-side (portal-list.js).
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    for r in &rows {
        fields
            .entry(r.field.clone())
            .or_insert_with(|| r.field_label.clone());
    }

    // The search haystack's descriptions (B1), by row id.
    let descs = crate::monitoring::Descriptions::load(&state.db, &snapshot)
        .await
        .by_id(rows.iter().map(|r| r.id.as_str()));

    // §9.1: the stat row counts these rows and its tiles filter this page.
    let stats = context! {
        total => rows.len(),
        online => rows.iter().filter(|r| r.connected).count(),
        oor => rows.iter().filter(|r| r.oor != "normal").count(),
        fault => rows.iter().filter(|r| !r.connected).count(),
        unset => rows.iter().filter(|r| r.state == "unset").count(),
    };

    // GitLab #109: a limit is written to its Hub as a signed command.
    let hub_names: Vec<String> = hubs.iter().map(|d| d.hostname.clone()).collect();
    let unpaired = super::unpaired_banner(&state, &current_user, &hub_names, "/limits").await;

    let ctx = render::page_ctx(
        context! {
            nav_active => "limits",
            unpaired => unpaired,
            descs => descs,
            rows => rows,
            fields => quantity_options(fields),
            limit_stats => stats,
            many_hubs => hubs.len() > 1,
            can_edit => render::can_edit_thresholds(&current_user),
        },
        Some(&current_user),
    );
    super::render(&state, "pages/limits.html", ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(state: &str, error: Option<&str>) -> PendingConfigRow {
        PendingConfigRow {
            hostname: "hub".into(),
            node_key: "aa".into(),
            param_key: "threshold.temperature.warning_high".into(),
            kind: "threshold".into(),
            desired_value: Some("8.0".into()),
            author: "eva".into(),
            created_at: 0,
            state: state.into(),
            attempts: 0,
            last_error: error.map(str::to_string),
            updated_at: 0,
            previous_value: None,
            previous_band: None,
            previous_order: None,
        }
    }

    /// The Node's value beside a drafted bound is the one before the send, not the new one
    /// the Hub already wrote into `field_thresholds`.
    #[test]
    fn the_node_holds_the_bound_from_before_the_send() {
        let drafted = pending("pending", None);
        assert_eq!(held_by_node(&drafted, Some("-10.0".into()), |v| v), "-10.0");
        let sent = PendingConfigRow {
            previous_value: Some("-10.0".into()),
            ..pending("sending", None)
        };
        assert_eq!(held_by_node(&sent, Some("-14.0".into()), |v| v), "-10.0");
        let unset_before = PendingConfigRow {
            previous_value: Some("null".into()),
            ..pending("sending", None)
        };
        assert_eq!(
            held_by_node(&unset_before, Some("-14.0".into()), |v| v),
            "—"
        );
        assert_eq!(held_by_node(&drafted, None, |v| v), "—");
    }

    #[test]
    fn a_refusal_outranks_an_offline_hub_and_a_send() {
        let refused = pending(PENDING_STATE_ERROR, Some("slot full"));
        let sending = pending(PENDING_STATE_SENDING, None);
        assert_eq!(
            drafted(&[&sending, &refused], false),
            Some(("error", Some("slot full".into())))
        );
        assert_eq!(drafted(&[&sending], false), Some(("offline", None)));
        assert_eq!(drafted(&[&sending], true).map(|d| d.0), Some("sending"));
        assert_eq!(drafted(&[], true), None);
    }

    #[test]
    fn an_undelivered_bound_is_neither_a_draft_nor_a_refusal() {
        let undelivered = pending(PENDING_STATE_UNDELIVERED, Some("no ack"));
        let sending = pending(PENDING_STATE_SENDING, None);
        let refused = pending(PENDING_STATE_ERROR, Some("slot full"));
        assert_eq!(
            drafted(&[&sending, &undelivered], false),
            Some(("undelivered", Some("no ack".into())))
        );
        assert_eq!(
            drafted(&[&undelivered, &refused], true).map(|d| d.0),
            Some("error")
        );
    }

    #[test]
    fn a_switched_off_band_is_not_in_force() {
        let b = (Some(0.0), None, None, Some(10.0));
        assert_eq!(settled(b, true), "ok");
        assert_eq!(settled(b, false), "off");
        assert_eq!(settled(Default::default(), true), "unset");
    }

    /// #167: a wired probe whose band is switched off is listed "off" (vypnuto), with its
    /// bounds still shown; an older Hub that sends no `enabled` is "ok".
    #[test]
    fn a_switched_off_probe_band_is_listed_off() {
        let mut device = DeviceState::new("proximos-00000001".into());
        let band = |enabled| crate::state::SensorThresholds {
            critical_low: Some(2.0),
            warning_low: Some(3.0),
            warning_high: Some(8.0),
            critical_high: Some(10.0),
            alarm_low: Some(2.0),
            alarm_high: Some(10.0),
            hysteresis: Some(0.5),
            enabled,
        };
        let rows = HubRows {
            node: HashMap::new(),
            eye: HashMap::new(),
            pending: HashMap::new(),
            delivery: HashMap::new(),
        };
        device.sensors[0].thresholds = Some(band(Some(false)));
        let row = device_rows(&device, &rows).remove(0);
        assert_eq!(row.kind, "probe");
        assert_eq!(row.state, "off");
        assert_eq!(row.bounds[3].as_deref(), Some("10.0"));
        assert!(!row.stored.as_ref().unwrap().enabled);

        device.sensors[0].thresholds = Some(band(None));
        assert_eq!(device_rows(&device, &rows).remove(0).state, "ok");
    }

    /// §9.4: the editor opens a field with no band on its default, switched off; a stored
    /// band as stored; a wired line never gets a default band.
    #[test]
    fn the_editor_opens_on_the_stored_band_or_the_default_switched_off() {
        let fresh = editor_band("eye", "temperature", None);
        assert!(!fresh.enabled);
        assert!(fresh.critical_low.is_some());
        let stored = editor_band(
            "eye",
            "temperature",
            Some(((Some(1.0), None, None, Some(9.0)), true, Some(0.2))),
        );
        assert_eq!(
            (
                stored.critical_low,
                stored.critical_high,
                stored.enabled,
                stored.hysteresis
            ),
            (Some(1.0), Some(9.0), true, 0.2)
        );
        let wired = editor_band("hub", "temperature", None);
        assert_eq!(wired.critical_low, None);
    }
}
