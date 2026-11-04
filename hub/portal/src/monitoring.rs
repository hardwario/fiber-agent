//! The `/monitoring` page's own context model: groups, rows and value cards.
//!
//! Its own module rather than more of `render.rs` because it introduces a
//! shape nothing else in the portal uses. Everywhere else a row is an
//! entity — one wired probe, one Node, one Beacon. Here the unit of display
//! is a **card**: one measured value with its own unit, alarm state and
//! threshold band. A wired probe is one card. A Node reporting temperature,
//! humidity and pressure is three. v1 does the same expansion in
//! `lib/group-cards.ts` and then merges a Node's cards back into one row
//! (`mergeStickerFieldRows`) so the table doesn't repeat its name three
//! times; [`GroupRowCtx`] is that merged row, carrying its cards in
//! `fields`.
//!
//! Which fields a Node or Beacon expands to:
//! - the member's own `fields` selection when it has one,
//! - otherwise every registry field it currently reports that is
//!   `Continuous` and `thresholdable`, minus battery — battery has its own
//!   column and a card for it would crowd out the readings (v1 excludes it
//!   in exactly the same place, and its threshold alarm still fires).

use serde::Serialize;

use portal_storage::{FieldThreshold, SensorGroup, SensorGroupMember};

use crate::notify::offline::{DeviceScope, LivenessVerdicts};
use crate::render::unix_to_rfc3339;
use crate::render::{
    displayed_node_thresholds, node_connected, normalize_radio_alarm_state, radio_connected,
    ThresholdEntryCtx, ThresholdsCtx,
};
use crate::state::{BeaconState, ChangeEvent, DeviceState, NodeState, PortalState, SensorState};

// The detector's verdict table ([`LivenessVerdicts`], as
// `OfflineService::liveness_snapshot` returns it) is threaded into
// [`sections`] and [`picker`] rather than read from the service there, so a
// render stays a pure function of a snapshot and one table can serve both a
// page and a fragment.
//
// MB2 introduced it here under the alias `OfflineSet`, a leftover from the
// WIP's plain offline *set*: a set can only answer the fault axis, and the
// rows take `connected` from the detector too (devices.backend.md §19.1's
// F2/F3 residue), which needs the third answer "I have no verdict" — the one
// case in which the Hub's echoed `alarm_state` is still consulted (see
// [`node_connected`]). The alias outlived the shape it was hiding and went
// in review round 2; the call sites name `LivenessVerdicts`.

/// The operational faults a group row can carry, as the row's `faults` and
/// the tooltip's i18n key spell them (`monitoring.groups.fault_{slug}`).
///
/// Named constants rather than bare literals because the key is built
/// dynamically in the template, which puts it out of reach of
/// `every_static_template_key_exists` — F9 was exactly that hole, and the
/// first faulty row showed the operator `monitoring.groups.fault_offline`
/// verbatim. `every_fault_tooltip_key_exists_in_both_catalogues` resolves
/// every one of these in both languages, so **a new fault belongs in that
/// list too**.
pub const FAULT_OFFLINE: &str = "offline";
pub const FAULT_LOW_BATTERY: &str = "low_battery";
/// The Node has not confirmed its settings within 4 h (config sync Q20c).
pub const FAULT_CONFIG_SYNC: &str = "config_sync";

/// The four layouts the view-mode dropdown offers, v1's
/// `MonitoringDisplayMode`. Order is the dropdown's order.
///
/// `table-simplified` was removed on 2026-10-15 (monitoring.md); a stored
/// or posted value of it maps to `table-detailed` ([`canonical_view_mode`]). The V8 CHECK still allows it.
pub const VIEW_MODES: [&str; 3] = ["card-detailed", "card-simplified", "table-detailed"];

/// What an unrecognised or absent stored mode falls back to — v1's default
/// for a returning user with nothing saved.
pub const DEFAULT_VIEW_MODE: &str = "card-detailed";

/// `true` for a mode this page can render. The `users` column has the same
/// CHECK constraint (migration V8); this is the layer that turns a bad value
/// into a 400 instead of a database error.
#[cfg(test)]
pub fn is_view_mode(mode: &str) -> bool {
    VIEW_MODES.contains(&mode)
}

/// The mode a requested layout resolves to: itself when offered, the
/// detailed table for the removed `table-simplified` (2026-10-15), `None`
/// for anything else.
pub fn canonical_view_mode(mode: &str) -> Option<&'static str> {
    if mode == "table-simplified" {
        return Some("table-detailed");
    }
    VIEW_MODES.iter().copied().find(|m| *m == mode)
}

// ---------------------------------------------------------------------
// Context shapes
// ---------------------------------------------------------------------

/// One measured value inside a row: a wired probe's temperature, or one of a
/// Node's or Beacon's fields.
#[derive(Debug, Clone, Serialize)]
pub struct GroupFieldCtx {
    /// Registry field name (`"temperature"`), or `None` for a wired probe —
    /// a probe measures one thing and has no field identity to show.
    pub field: Option<String>,
    /// Human label for the field, `None` for a wired probe (same reason).
    pub label: Option<String>,
    pub value: Option<f64>,
    pub unit: String,
    /// Normalised (`"Normal"`/`"Warning"`/…), never the firmware casing —
    /// the templates compare against the normalised spelling everywhere.
    pub alarm_state: String,
    /// Either shape the `ui.threshold_gauge` macro accepts; it reads the four
    /// bounds defensively and draws its empty bar when they are missing.
    pub thresholds: Option<ThresholdEntryCtx>,
    /// Wired probes carry the richer wired threshold shape (it has the two
    /// extra alarm bounds), so they fill this instead of `thresholds`.
    pub wired_thresholds: Option<ThresholdsCtx>,
    /// [`GroupFieldCtx::bounded`] over the two threshold shapes above, set by
    /// [`GroupFieldCtx::new`]. Serialized so the table's value pill stays
    /// neutral for this one field even when a sibling field is bounded.
    pub has_limits: bool,
    /// Recent history as a ready-to-draw polyline — only the detailed card
    /// layout asks for it, and only [`fill_sparklines`] ever sets it.
    pub spark: Option<SparkCtx>,
}

impl GroupFieldCtx {
    /// Fills `has_limits` from the thresholds; `spark` starts empty.
    fn new(
        field: Option<String>,
        label: Option<String>,
        value: Option<f64>,
        unit: String,
        alarm_state: String,
        thresholds: Option<ThresholdEntryCtx>,
        wired_thresholds: Option<ThresholdsCtx>,
    ) -> Self {
        Self {
            has_limits: Self::bounded(thresholds.as_ref(), wired_thresholds.as_ref()),
            field,
            label,
            value,
            unit,
            alarm_state,
            thresholds,
            wired_thresholds,
            spark: None,
        }
    }

    /// Whether this card has a bound for its value to be within: a
    /// switched-on limit set with at least one number in it. A field that is
    /// enabled with all four bounds cleared (the Node/Beacon detail lets you
    /// do that) has nothing to be "normal" against, and reads "mez
    /// nenastavena" there (UX-033); this is the same rule for Monitoring
    /// (UX-055).
    ///
    /// A wired probe's alarm pair does not count (decision 2026-09-26): a v1
    /// leftover the device reads as a second warning bound, 0 / 100 °C by
    /// default, so it alone would make every line look bounded.
    fn bounded(thresholds: Option<&ThresholdEntryCtx>, wired: Option<&ThresholdsCtx>) -> bool {
        thresholds.is_some_and(ThresholdEntryCtx::bounded)
            || wired.is_some_and(|t| {
                [
                    t.critical_low,
                    t.warning_low,
                    t.warning_high,
                    t.critical_high,
                ]
                .iter()
                .any(Option::is_some)
            })
    }
}

/// The detailed card's trend line. Built server-side into viewBox
/// coordinates so the template is one `<polyline points="…">` and the page
/// ships no charting code — the history page's ECharts island is for
/// interrogating data, this is for glancing at a shape.
#[derive(Debug, Clone, Serialize)]
pub struct SparkCtx {
    /// `"x,y x,y …"` in a 100x32 viewBox, oldest point first. y is inverted
    /// (SVG's origin is top-left) so the line reads the way a chart does.
    pub points: String,
    pub min: f64,
    pub max: f64,
}

/// At most this many points per line — beyond it the polyline is longer than
/// the card is wide in pixels, so the extra precision is invisible and only
/// costs HTML. A longer series is downsampled to bucket means; min and max
/// still come from every reading (the card prints them as the 24 h range).
const SPARK_POINTS: usize = 48;

impl SparkCtx {
    /// `None` for fewer than two readings (a single dot is not a trend) or a
    /// flat line with no spread to normalise against.
    pub fn from_values(values: &[f64]) -> Option<Self> {
        if values.len() < 2 {
            return None;
        }
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        if !min.is_finite() || !max.is_finite() {
            return None;
        }
        let values: Vec<f64> = if values.len() > SPARK_POINTS {
            (0..SPARK_POINTS)
                .map(|b| {
                    let lo = b * values.len() / SPARK_POINTS;
                    let hi = ((b + 1) * values.len() / SPARK_POINTS).max(lo + 1);
                    let chunk = &values[lo..hi];
                    chunk.iter().sum::<f64>() / chunk.len() as f64
                })
                .collect()
        } else {
            values.to_vec()
        };
        // A perfectly flat series still deserves a line; draw it mid-height
        // rather than dividing by a zero span.
        let span = if (max - min).abs() < f64::EPSILON {
            None
        } else {
            Some(max - min)
        };
        let step = 100.0 / (values.len() - 1) as f64;
        let points = values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let y = match span {
                    Some(span) => 30.0 - ((v - min) / span) * 28.0,
                    None => 16.0,
                };
                format!("{:.1},{:.1}", i as f64 * step, y)
            })
            .collect::<Vec<_>>()
            .join(" ");
        Some(SparkCtx { points, min, max })
    }
}

/// A wired probe's own page (devices.hub.md §3.2, amendment 2026-10-14):
/// 1-based in the URL like the UI, `line` is the wire's 0..7.
pub fn probe_href(hostname: &str, line: u8) -> String {
    format!("/hubs/{hostname}/sensors/line-{}", u16::from(line) + 1)
}

/// One entity's row: its identity, its status, and its value cards.
#[derive(Debug, Clone, Serialize)]
pub struct GroupRowCtx {
    /// `"probe"` | `"node"` | `"beacon"` — drives the icon and the type filter.
    pub kind: &'static str,
    /// The section this row belongs to. On the row rather than plumbed
    /// through template context because a row is included from three
    /// different nesting depths, and minijinja does not carry a caller's
    /// outer `{% set %}` locals into an include (see
    /// `partials/group_table.html`'s header).
    pub group_id: i64,
    /// The member token — `probe:{line}` / `node:{key}` / `beacon:{mac}`,
    /// the same encoding the picker and the exclusivity check use. It
    /// addresses this row's fragment route, and it is what
    /// [`fill_sparklines`] recovers identity from.
    ///
    /// URL-safe as it stands: hex, digits and colons only, all valid in a
    /// path segment.
    pub token: String,
    /// The SSE event name this row listens on: `mon-{kind}-{host}-{identity}`.
    ///
    /// **Entity-level, not row-level.** One reading nudges every group that
    /// holds any part of that entity, and each of them refetches its own
    /// row. Deriving it from the entity is what lets MB2 add no
    /// `ChangeEvent` variant: the events that already fire on every reading
    /// and every offline transition carry exactly this identity.
    pub event: String,
    /// Stable DOM id, unique across the page.
    ///
    /// It carries the **group id** because since MB1 exclusivity is per
    /// field, so one Node legitimately appears in two sections — its
    /// temperature in group A, its humidity in group B. Without the group id
    /// those two `<tr>`s would share an id, and htmx swaps by id.
    pub row_id: String,
    pub hostname: String,
    pub device_label: String,
    pub name: String,
    /// The entity's location, shown as the Description column.
    pub description: Option<String>,
    pub connected: bool,
    /// The row's Hub is offline, so nothing is known about a Node or Beacon
    /// behind it (#30c): the row reads "neznámý", is not `connected` and
    /// carries no fault of its own; the group's banner names the Hub. Never
    /// set on a wired probe, which reads not connected (UX-053).
    pub unknown: bool,
    /// The **value** class only: the worst of this row's cards, `"Normal"`
    /// when it has none.
    ///
    /// Never a connectivity state. Whether the entity is reachable lives on
    /// `connected` and nowhere else (MB0, monitoring.backend.md §3 WP0.2):
    /// the firmware enum folds the two axes into one string, and a row that
    /// borrowed `Disconnected` for this field made a silent Node
    /// indistinguishable from an in-range one, and dropped a genuinely
    /// alarming silent Node out of the "Mimo meze" filter.
    pub alarm_state: String,
    /// Any card of this row has a limit to be within
    /// ([`GroupFieldCtx::bounded`]). A `"Normal"` row without one is not
    /// a verdict, and the templates say "mez nenastavena" instead of
    /// "Vše v normálu" / "Normální".
    pub has_limits: bool,
    pub last_seen: Option<String>,
    /// "less than a minute ago" / "3 hours ago", already translated — the
    /// detailed card's trend line carries it under the graph (v1's
    /// TrendGraph `lastUpdate`). Coarse buckets on purpose: it answers "is
    /// this reading fresh", and a live second counter would need JS.
    pub last_seen_rel: Option<String>,
    /// `last_seen` as unix seconds, for the detailed card's short age
    /// ("40 s"), which the page re-counts client-side (variant B, 2026-10-15).
    pub last_seen_unix: Option<i64>,
    /// [`short_age`] of `last_seen_unix`, the server's first paint of it.
    pub last_seen_short: Option<String>,
    /// The Hub echoes `Reconnecting`: silent past ~2× its interval though
    /// still counted connected — the device tables' "late" dot.
    pub reconnecting: bool,
    /// Pre-formatted for display ("3.60 V", "2.95 V"); the column shows it
    /// verbatim. Both kinds are volts: a Node's from its `voltage` registry
    /// field, a Beacon's through [`crate::render::beacon_battery_volts`],
    /// which converts the wire's millivolts.
    pub battery: Option<String>,
    /// Pre-formatted signal ("-92 dBm / 7.5 dB"), `None` for wired probes.
    pub signal: Option<String>,
    pub fields: Vec<GroupFieldCtx>,
    /// Lower-cased haystack for the toolbar's client-side search.
    pub search: String,
    /// Operational-class faults: `"offline"`, `"low_battery"`. The *other*
    /// axis from `alarm_state`, which is the value class — §3.1's "dva
    /// nezávislé indikátory … nikdy se neslévají".
    ///
    /// Empty for a wired probe by design: it has no per-probe liveness of
    /// its own, only the Hub-level `DeviceOffline`, which is a different
    /// scope and must not be rendered as if the probe itself were silent
    /// (monitoring.backend.md §9.2).
    pub faults: Vec<&'static str>,
}

/// One group's section of the page.
#[derive(Debug, Clone, Serialize)]
pub struct GroupSectionCtx {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    /// The group's location (V40), muted next to the name; `None` hides it.
    pub location: Option<String>,
    /// Members the Hub is currently reporting. Empty is normal and rendered
    /// as such: a group whose Node dropped off the network is still a group.
    pub rows: Vec<GroupRowCtx>,
    /// How many **value cards** the group holds — not how many probes.
    /// Three Nodes reporting T+H is "6 čidel", per
    /// [monitoring.md §3.1](../../plan/portal/monitoring.md).
    pub field_count: usize,
    /// Worst value class across every field of every row, `"Normal"` when
    /// nothing is out of range. Drives the header's accent strip and the
    /// tone of its status badge. Never a connectivity state — MB0 split
    /// those onto `GroupRowCtx::connected`.
    pub alarm_state: String,
    /// Fields at Warning or Critical. The "Mimo meze: N" badge is hidden at
    /// zero.
    pub out_of_range: usize,
    /// Rows carrying an operational fault.
    pub fault_count: usize,
    /// Every event name this section's rows listen on, deduplicated. The
    /// header refetches itself on any of them, because its counts are
    /// aggregates over the rows.
    pub member_events: Vec<String>,
    /// The offline Hubs this group's rows belong to, one banner line each
    /// (#30c), in the order their first row appears.
    pub offline_hubs: Vec<OfflineHubCtx>,
}

/// One "Hub … je offline od …" line in a group header (#30c).
#[derive(Debug, Clone, Serialize)]
pub struct OfflineHubCtx {
    /// The Hub's display label, the same one the rows' Device column shows.
    pub name: String,
    /// RFC 3339 of the last message heard from the Hub
    /// (`DeviceState::last_seen`), for `short_dt`; `None` when it never
    /// reported in this process.
    pub since: Option<String>,
}

/// The four aggregates every group header shows.
///
/// **These are group totals, never filtered counts.** The toolbar's
/// client-side filter (`pages/dashboard.html`) hides rows and whole sections
/// in the DOM and deliberately does not touch the badges: a header that
/// re-counted under a filter would answer "how many are you showing me"
/// when the operator asked "how big is this group, and is any of it out of
/// range" — and a collapsed group (§3.2) must still inform.
fn aggregate(rows: &[GroupRowCtx]) -> (usize, String, usize, usize) {
    let states: Vec<String> = rows
        .iter()
        .flat_map(|r| r.fields.iter().map(|f| f.alarm_state.clone()))
        .collect();
    let out_of_range = states
        .iter()
        .filter(|s| s.as_str() == "Warning" || s.as_str() == "Critical")
        .count();
    // One faulty row counts once however many faults it carries and however
    // many cards it shows: the header answers "how many probes are in
    // trouble", not "how many things are wrong".
    let fault_count = rows.iter().filter(|r| !r.faults.is_empty()).count();
    (
        states.len(),
        worst(&states).unwrap_or_else(|| "Normal".to_string()),
        out_of_range,
        fault_count,
    )
}

// ---------------------------------------------------------------------
// Row builders
// ---------------------------------------------------------------------

/// The worst **value** state across a row's cards, `None` when none of them
/// says anything about range.
///
/// The three connectivity states rank 0 rather than above `Normal`: since MB0
/// they belong to `connected`, not here. A field carrying one is therefore
/// not "worse than normal", it is simply not a verdict on the value.
fn worst(states: &[String]) -> Option<String> {
    let rank = |s: &str| match s {
        "Critical" => 4,
        "Warning" => 3,
        _ => 0,
    };
    states
        .iter()
        .max_by_key(|s| rank(s))
        .filter(|s| rank(s) > 0)
        .cloned()
}

/// The value half of a normalized firmware `alarm_state`. The connectivity
/// states carry no verdict on the reading, so they read as `"Normal"` here
/// and are answered by `connected` instead.
fn value_class(normalized_alarm_state: &str) -> String {
    if radio_connected(normalized_alarm_state) {
        normalized_alarm_state.to_string()
    } else {
        "Normal".to_string()
    }
}

/// `hub_online` gates `connected`: the Hub is the only voice a wired probe
/// has, so an offline Hub's probes are not live (UX-053, the same gate as
/// the fleet counter in `render::stats`).
pub fn probe_row(
    group_id: i64,
    hostname: &str,
    device_label: &str,
    s: &SensorState,
    hub_online: bool,
) -> GroupRowCtx {
    // A wired probe reports `connected` on the model, so the connectivity
    // half of `dominant_alarm_state` is redundant here — only its value half
    // is kept, for the row and for its single card alike.
    let state = value_class(&normalize_radio_alarm_state(&s.dominant_alarm_state));
    let name = s.name.clone().unwrap_or_else(|| format!("Line {}", s.line));
    let fields = vec![GroupFieldCtx::new(
        None,
        None,
        s.temperature_avg_celsius,
        "°C".to_string(),
        state.clone(),
        None,
        s.thresholds.as_ref().map(ThresholdsCtx::from),
    )];
    GroupRowCtx {
        has_limits: fields.iter().any(|f| f.has_limits),
        kind: "probe",
        group_id,
        token: format!("probe:{}", s.line),
        event: probe_event(hostname, s.line),
        // A wired probe carries no per-probe fault: its only liveness signal
        // is the Hub's own `DeviceOffline`, a different scope (§9.2).
        faults: Vec::new(),
        row_id: format!("row-{group_id}-probe-{hostname}-{}", s.line),
        hostname: hostname.to_string(),
        device_label: device_label.to_string(),
        search: search_haystack(&[Some(&name), s.location.as_deref(), Some(device_label)]),
        name,
        description: s.location.clone(),
        connected: s.connected && hub_online,
        unknown: false,
        alarm_state: state,
        last_seen: s.updated_at.and_then(unix_to_rfc3339),
        last_seen_rel: s.updated_at.map(relative_time),
        last_seen_unix: s.updated_at,
        last_seen_short: s.updated_at.map(short_age),
        reconnecting: false,
        battery: None,
        signal: None,
        fields,
    }
}

/// `selected` is the member's `fields` list; `None` means every reporting
/// continuous+thresholdable field except battery (see the module doc).
pub fn node_row(
    group_id: i64,
    hostname: &str,
    device_label: &str,
    n: &NodeState,
    selected: Option<&[String]>,
    liveness: &LivenessVerdicts,
) -> GroupRowCtx {
    // The detector overrides the Hub's echo, exactly as `/nodes` does — one
    // liveness verdict for the whole portal (devices.backend.md §19.1, F2 +
    // F3). The echo survives only where the detector holds no verdict.
    //
    // Asked once and both axes derived from it: the fault indicator used to
    // repeat the lookup, which is two table reads per row for one answer.
    let verdict = liveness.verdict(DeviceScope::Node, &n.node_key);
    let connected = node_connected(&normalize_radio_alarm_state(&n.alarm_state), verdict);
    let thresholds = displayed_node_thresholds(n.field_thresholds.as_deref(), &[]);

    let fields: Vec<_> = fiber_protocol::lorawan_registry()
        .iter()
        .filter(|f| {
            f.kind == fiber_protocol::LorawanFieldKind::Continuous
                && f.thresholdable
                && f.group != fiber_protocol::LorawanFieldGroup::Battery
        })
        .filter(|f| selected.is_none_or(|sel| sel.iter().any(|s| s == f.name)))
        .filter_map(|f| {
            n.fields.get(f.name).map(|&value| {
                GroupFieldCtx::new(
                    Some(f.name.to_string()),
                    Some(f.label.to_string()),
                    Some(f.to_display(value)),
                    f.display_unit().unwrap_or("").to_string(),
                    n.field_alarm_states
                        .get(f.name)
                        .map(|s| normalize_radio_alarm_state(s))
                        .unwrap_or_else(|| "Normal".to_string()),
                    thresholds.get(f.name).cloned(),
                    None,
                )
            })
        })
        .collect();

    let name = n.name.clone().unwrap_or_else(|| n.node_key.clone());
    let mut faults = Vec::new();
    // `verdict == Some(true)`, not `!connected`: a Node the detector has
    // never measured is not a fault, however the echo reads it (§9.2,
    // `NeverConnected` is a lifecycle state, not a silence).
    if verdict == Some(true) {
        faults.push(FAULT_OFFLINE);
    }
    if crate::pending::has_config_sync_fault(&n.node_key) {
        faults.push(FAULT_CONFIG_SYNC);
    }
    GroupRowCtx {
        kind: "node",
        has_limits: fields.iter().any(|f| f.has_limits),
        group_id,
        token: format!("node:{}", n.node_key),
        event: node_event(hostname, &n.node_key),
        faults,
        row_id: format!("row-{group_id}-node-{hostname}-{}", n.node_key),
        hostname: hostname.to_string(),
        device_label: device_label.to_string(),
        search: search_haystack(&[
            Some(&name),
            Some(&n.node_key),
            n.serial_number.as_deref(),
            n.location.as_deref(),
            Some(device_label),
        ]),
        name,
        description: n.location.clone(),
        connected,
        unknown: false,
        alarm_state: worst(
            &fields
                .iter()
                .map(|f| f.alarm_state.clone())
                .collect::<Vec<_>>(),
        )
        // No cards means nothing is out of range — not "borrow whatever the
        // entity's own enum happens to say", which for a silent Node was
        // `Disconnected`.
        .unwrap_or_else(|| "Normal".to_string()),
        last_seen: n.last_seen.clone(),
        last_seen_rel: rfc3339_to_unix(n.last_seen.as_deref()).map(relative_time),
        last_seen_unix: rfc3339_to_unix(n.last_seen.as_deref()),
        last_seen_short: rfc3339_to_unix(n.last_seen.as_deref()).map(short_age),
        reconnecting: normalize_radio_alarm_state(&n.alarm_state) == "Reconnecting",
        battery: n.fields.get("voltage").map(|v| format!("{v:.2} V")),
        signal: radio_signal(n.rssi, n.snr),
        fields,
    }
}

pub fn beacon_row(
    group_id: i64,
    hostname: &str,
    device_label: &str,
    b: &BeaconState,
    db_thresholds: &[FieldThreshold],
    selected: Option<&[String]>,
    liveness: &LivenessVerdicts,
) -> GroupRowCtx {
    // Same rule as the Node row above, asked once for both axes: the
    // detector's verdict wins, the Hub's echo is only the fallback where it
    // holds none.
    let verdict = liveness.verdict(DeviceScope::Beacon, &b.mac);
    let connected = node_connected(&normalize_radio_alarm_state(&b.alarm_state), verdict);

    // The EYE registry's four fields are temperature, humidity, battery and
    // movement; only the first two are readings a card should show.
    let fields: Vec<_> = fiber_protocol::eye_registry()
        .iter()
        .filter(|f| f.thresholdable && f.name != "battery")
        .filter(|f| selected.is_none_or(|sel| sel.iter().any(|s| s == f.name)))
        .filter_map(|f| {
            let value = match f.name {
                "temperature" => b.temperature,
                "humidity" => b.humidity.map(|h| h as f64),
                _ => None,
            }?;
            Some(GroupFieldCtx::new(
                Some(f.name.to_string()),
                Some(f.label.to_string()),
                Some(value),
                f.unit.unwrap_or("").to_string(),
                b.field_alarm_states
                    .get(f.name)
                    .map(|s| normalize_radio_alarm_state(s))
                    .unwrap_or_else(|| "Normal".to_string()),
                db_thresholds
                    .iter()
                    .find(|t| t.field == f.name)
                    .map(ThresholdEntryCtx::from),
                None,
            ))
        })
        .collect();

    let name = b.name.clone().unwrap_or_else(|| b.mac.clone());
    let mut faults = Vec::new();
    if verdict == Some(true) {
        faults.push(FAULT_OFFLINE);
    }
    // The wire flag, not a portal-side threshold: a Beacon reports this
    // itself. There is no equivalent for a Node — recorded as a gap in
    // monitoring.backend.md §15.
    if b.low_battery == Some(true) {
        faults.push(FAULT_LOW_BATTERY);
    }
    GroupRowCtx {
        kind: "beacon",
        has_limits: fields.iter().any(|f| f.has_limits),
        group_id,
        token: format!("beacon:{}", b.mac),
        event: beacon_event(hostname, &b.mac),
        faults,
        row_id: format!(
            "row-{group_id}-beacon-{hostname}-{}",
            b.mac.replace(':', "-")
        ),
        hostname: hostname.to_string(),
        device_label: device_label.to_string(),
        search: search_haystack(&[
            Some(&name),
            Some(&b.mac),
            b.location.as_deref(),
            Some(device_label),
        ]),
        name,
        description: b.location.clone(),
        connected,
        unknown: false,
        alarm_state: worst(
            &fields
                .iter()
                .map(|f| f.alarm_state.clone())
                .collect::<Vec<_>>(),
        )
        // No cards means nothing is out of range — not "borrow whatever the
        // entity's own enum happens to say", which for a silent Node was
        // `Disconnected`.
        .unwrap_or_else(|| "Normal".to_string()),
        last_seen: b.last_seen.and_then(crate::render::unix_to_rfc3339),
        last_seen_rel: b.last_seen.map(relative_time),
        last_seen_unix: b.last_seen,
        last_seen_short: b.last_seen.map(short_age),
        reconnecting: normalize_radio_alarm_state(&b.alarm_state) == "Reconnecting",
        battery: crate::render::beacon_battery_volts(b.battery_mv),
        signal: radio_signal(b.rssi, None),
        fields,
    }
}

/// Coarse "how long ago", in the request's language. Buckets rather than an
/// exact duration: the card asks whether a reading is fresh, and anything
/// finer would go stale the moment the page stopped moving.
pub(crate) fn relative_time(ts: i64) -> String {
    let secs = (crate::render::now_unix() - ts).max(0);
    let tr = |key: &str| crate::i18n::lookup(crate::i18n::current_lang(), key);
    let plural = |key: &str, n: i64| tr(key).replace("{n}", &n.to_string());
    match secs {
        0..=59 => tr("monitoring.groups.rel_under_minute"),
        60..=3599 => plural("monitoring.groups.rel_minutes", secs / 60),
        3600..=86399 => plural("monitoring.groups.rel_hours", secs / 3600),
        _ => plural("monitoring.groups.rel_days", secs / 86400),
    }
}

/// "40 s" / "5 min" / "2 h" / "3 d": the detailed card's data age.
/// `static/js` re-counts it from `data-age-ts` with the same buckets.
pub(crate) fn short_age(ts: i64) -> String {
    let secs = (crate::render::now_unix() - ts).max(0);
    match secs {
        0..=59 => format!("{secs} s"),
        60..=3599 => format!("{} min", secs / 60),
        3600..=86399 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86400),
    }
}

/// Node `last_seen` is an RFC 3339 string straight off the wire, so the
/// relative label has to go back through a parse the other two kinds skip.
fn rfc3339_to_unix(s: Option<&str>) -> Option<i64> {
    let s = s?;
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp())
}

fn radio_signal(rssi: Option<i64>, snr: Option<f64>) -> Option<String> {
    match (rssi, snr) {
        (Some(r), Some(s)) => Some(format!("{r} dBm / {s} dB")),
        (Some(r), None) => Some(format!("{r} dBm")),
        (None, Some(s)) => Some(format!("{s} dB")),
        (None, None) => None,
    }
}

fn search_haystack(parts: &[Option<&str>]) -> String {
    parts
        .iter()
        .flatten()
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

// ---------------------------------------------------------------------
// Page assembly
// ---------------------------------------------------------------------

/// Beacon thresholds, keyed by MAC — loaded once per request by the route
/// (they live in the database, not in the store).
pub type BeaconThresholds = std::collections::BTreeMap<String, Vec<FieldThreshold>>;

/// One section per group, in the order the repository returned them.
///
/// A sensor that is in no group does not appear: `/monitoring` shows what the
/// operator grouped and nothing else, as v1 does. An unassigned probe is
/// still on `/nodes`, `/beacons` and `/history` — it is only absent from the
/// view whose whole purpose is the operator's own arrangement.
///
/// A member naming an entity the Hub is not reporting (a Node that was
/// removed, a probe on a Hub that has not checked in) is skipped rather than
/// rendered as a ghost row — the group survives its members disappearing,
/// which is the whole reason membership is stored by identity and resolved
/// at render time.
pub fn sections(
    state: &PortalState,
    groups: &[SensorGroup],
    members: &[SensorGroupMember],
    beacon_thresholds: &BeaconThresholds,
    liveness: &LivenessVerdicts,
) -> Vec<GroupSectionCtx> {
    groups
        .iter()
        .map(|g| {
            let rows: Vec<GroupRowCtx> = members
                .iter()
                .filter(|m| m.group_id == g.id)
                .filter_map(|m| {
                    let device = state.device(&m.hostname)?;
                    resolve_member(g, device, m, beacon_thresholds, liveness)
                })
                .collect();
            let (field_count, alarm_state, out_of_range, fault_count) = aggregate(&rows);
            let mut offline_hubs: Vec<(String, OfflineHubCtx)> = Vec::new();
            for r in &rows {
                let Some(d) = state.device(&r.hostname).filter(|d| !d.online) else {
                    continue;
                };
                if !offline_hubs.iter().any(|(h, _)| *h == d.hostname) {
                    offline_hubs.push((
                        d.hostname.clone(),
                        OfflineHubCtx {
                            name: device_label(d),
                            since: d.last_seen.and_then(unix_to_rfc3339),
                        },
                    ));
                }
            }
            // Deduplicated: the same entity cannot appear twice in one group
            // today, but a header listening on a name twice would refetch
            // twice for one nudge.
            let mut member_events: Vec<String> = rows.iter().map(|r| r.event.clone()).collect();
            member_events.sort();
            member_events.dedup();
            GroupSectionCtx {
                id: g.id,
                name: g.name.clone(),
                description: g.description.clone(),
                location: g.location.clone(),
                rows,
                field_count,
                alarm_state,
                out_of_range,
                fault_count,
                member_events,
                offline_hubs: offline_hubs.into_iter().map(|(_, o)| o).collect(),
            }
        })
        .collect()
}

/// The operator's free-text descriptions of Nodes, Beacons and Hubs
/// (`device_meta.description`), for the lists' fulltext search: every list
/// matches a device by its description as well as its name (B1). The store
/// does not carry descriptions, so a page reads them once per render.
#[derive(Debug, Default, Clone)]
pub(crate) struct Descriptions(std::collections::HashMap<String, String>);

impl Descriptions {
    /// Every description of every Hub the snapshot knows, by
    /// `node:<key lowercase>`, `beacon:<MAC uppercase>` and `hub:<hostname>`.
    pub(crate) async fn load(db: &crate::db::Db, state: &PortalState) -> Self {
        let hosts: Vec<String> = state.devices.values().map(|d| d.hostname.clone()).collect();
        let rows = db
            .run(move |s| {
                let mut out = Vec::new();
                for h in &hosts {
                    for scope in [
                        portal_storage::THRESHOLD_SCOPE_NODE,
                        portal_storage::THRESHOLD_SCOPE_EYE,
                        portal_storage::THRESHOLD_SCOPE_HUB,
                    ] {
                        out.extend(s.list_device_meta(h, scope)?);
                    }
                }
                Ok(out)
            })
            .await
            .unwrap_or_default();
        let mut map = std::collections::HashMap::new();
        for m in rows {
            let Some(d) = m.description.filter(|d| !d.trim().is_empty()) else {
                continue;
            };
            let key = match m.scope.as_str() {
                "node" => format!("node:{}", m.entity.to_lowercase()),
                "eye" => format!("beacon:{}", m.entity.to_uppercase()),
                "hub" => format!("hub:{}", m.hostname.to_lowercase()),
                _ => continue,
            };
            map.insert(key, d);
        }
        Descriptions(map)
    }

    /// The description of the device a row token or id names:
    /// `node:<key>[:field]`, `beacon:<MAC>[:field]` / `eye:<MAC>[:field]`,
    /// `hub:<hostname>`. `None` for a wired probe and for anything unknown.
    pub(crate) fn for_token(&self, token: &str) -> Option<&str> {
        let key = if let Some(rest) = token.strip_prefix("node:") {
            format!("node:{}", rest.split(':').next()?.to_lowercase())
        } else if let Some(rest) = token
            .strip_prefix("beacon:")
            .or_else(|| token.strip_prefix("eye:"))
        {
            // A MAC carries five colons of its own; a field may follow it.
            format!("beacon:{}", rest.get(..17).unwrap_or(rest).to_uppercase())
        } else if let Some(rest) = token.strip_prefix("hub:") {
            format!("hub:{}", rest.to_lowercase())
        } else {
            return None;
        };
        self.0.get(&key).map(String::as_str)
    }

    /// `row id -> description` for the ids that have one: what a template
    /// appends to a row's `data-search`.
    pub(crate) fn by_id<'a>(
        &self,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> std::collections::BTreeMap<String, String> {
        ids.into_iter()
            .filter_map(|id| Some((id.to_string(), self.for_token(id)?.to_string())))
            .collect()
    }

    /// Appends each Node and Beacon row's description to its haystack.
    pub(crate) fn add_to_row(&self, row: &mut GroupRowCtx) {
        if let Some(d) = self.for_token(&row.token) {
            row.search.push(' ');
            row.search.push_str(&d.to_lowercase());
        }
    }

    pub(crate) fn add_to_sections(&self, sections: &mut [GroupSectionCtx]) {
        for sec in sections {
            for row in &mut sec.rows {
                self.add_to_row(row);
            }
        }
    }
}

/// Resolves one stored membership against the live snapshot. `pub(crate)`
/// since MB2: the row fragment route builds exactly one row this way, so the
/// page and the fragment cannot diverge.
///
/// Takes the whole [`SensorGroup`] rather than its id because F1 puts the
/// group name into the row's search haystack — an operator typing a group
/// name expects its rows, and the row builders below know nothing about
/// groups beyond the id they stamp into the DOM.
pub(crate) fn resolve_member(
    group: &SensorGroup,
    device: &DeviceState,
    m: &SensorGroupMember,
    beacon_thresholds: &BeaconThresholds,
    liveness: &LivenessVerdicts,
) -> Option<GroupRowCtx> {
    let mut row = resolve_row(group.id, device, m, beacon_thresholds, liveness)?;
    row.search.push(' ');
    row.search.push_str(&group.name.to_lowercase());
    Some(row)
}

fn resolve_row(
    group_id: i64,
    device: &DeviceState,
    m: &SensorGroupMember,
    beacon_thresholds: &BeaconThresholds,
    liveness: &LivenessVerdicts,
) -> Option<GroupRowCtx> {
    let label = device_label(device);
    let selected = m.fields.as_deref();
    if let Some(line) = m.sensor_line {
        let s = device.sensors.iter().find(|s| i64::from(s.line) == line)?;
        return Some(probe_row(
            group_id,
            &device.hostname,
            &label,
            s,
            device.online,
        ));
    }
    if let Some(key) = &m.node_key {
        let n = device.nodes.get(key)?;
        let row = node_row(group_id, &device.hostname, &label, n, selected, liveness);
        return Some(behind_hub(row, device.online));
    }
    let mac = m.mac.as_ref()?;
    let b = device.ble_sensors.get(mac)?;
    if b.discovered == Some(true) {
        return None;
    }
    let thresholds = beacon_thresholds.get(mac).map(Vec::as_slice).unwrap_or(&[]);
    let row = beacon_row(
        group_id,
        &device.hostname,
        &label,
        b,
        thresholds,
        selected,
        liveness,
    );
    Some(behind_hub(row, device.online))
}

/// A Node or Beacon row behind an offline Hub reads "neznámý" at once
/// instead of waiting for its own offline detector (#30c): not connected,
/// and no fault of its own, since the group's banner names the Hub — the
/// same principle as a wired probe (UX-053).
fn behind_hub(mut row: GroupRowCtx, hub_online: bool) -> GroupRowCtx {
    if !hub_online {
        row.unknown = true;
        row.connected = false;
        row.faults.clear();
    }
    row
}

fn device_label(device: &DeviceState) -> String {
    device
        .system
        .device_label
        .clone()
        .unwrap_or_else(|| device.hostname.clone())
}

// ---------------------------------------------------------------------
// The New Group / edit dialog's picker
// ---------------------------------------------------------------------

/// One selectable field inside a Node or Beacon entry.
#[derive(Debug, Clone, Serialize)]
pub struct PickerFieldCtx {
    pub name: String,
    pub label: String,
    /// Name of the group that already holds **this field**, if any. Since
    /// MB1 exclusivity is per field, so a Node with its temperature taken
    /// and its humidity free renders selectable with one box disabled.
    pub in_group: Option<String>,
}

/// One entity the operator can tick. `token` is the value the form submits
/// and the key every other selection is expressed in — see
/// `routes::group_routes`'s header for the encoding.
#[derive(Debug, Clone, Serialize)]
pub struct PickerEntityCtx {
    pub token: String,
    pub kind: &'static str,
    pub name: String,
    /// The muted parenthetical after the name: `"2 čidla"`, or `None` for a
    /// wired probe, which measures one thing and needs no qualifier. The
    /// section heading already says Node or Beacon.
    pub detail: Option<String>,
    pub connected: bool,
    /// Name of the group holding **all** of this entity, if any — the
    /// condition for locking the whole row.
    ///
    /// Since MB1 that is narrower than "belongs to a group": an entity with
    /// one field taken and one free is still tickable, and the claim shows
    /// on the field box instead ([`PickerFieldCtx::in_group`]). It is `Some`
    /// when a `fields = NULL` membership holds the entity, when every offered
    /// field is individually claimed, or — for a wired probe, which offers no
    /// fields because it measures one thing — on any claim at all.
    ///
    /// A Node may now legitimately appear in two groups, which is why
    /// `GroupRowCtx::row_id` carries the group id.
    pub in_group: Option<String>,
    /// Empty for a wired probe: it measures one thing, there is nothing to
    /// choose. Non-empty entries drive the per-field checkboxes.
    pub fields: Vec<PickerFieldCtx>,
    /// The entity's location: the channel table's Location column and
    /// filter (monitoring.md, dodatek 2026-10-15).
    pub location: Option<String>,
}

/// The picker, grouped by Hub — v1's dialog has the same three sections per
/// device (probes, STICKERs, EYE BLE Tags).
#[derive(Debug, Clone, Serialize)]
pub struct PickerDeviceCtx {
    pub hostname: String,
    pub label: String,
    pub online: bool,
    pub probes: Vec<PickerEntityCtx>,
    pub nodes: Vec<PickerEntityCtx>,
    pub beacons: Vec<PickerEntityCtx>,
}

/// A group as the edit dialog needs it: its own fields plus the flat
/// selection lists the template checks membership against
/// (`token in g.selected`, `token ~ "|" ~ field in g.selected_fields`).
#[derive(Debug, Clone, Serialize)]
pub struct GroupEditCtx {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub selected: Vec<String>,
    pub selected_fields: Vec<String>,
    /// The group's location (V40). `group_edit_ctxs` leaves it `None`; the
    /// edit page fills it from `list_sensor_group_locations`.
    pub location: Option<String>,
    /// This dialog's own picker, built with this group's claims excluded —
    /// the page-level `picker` is the New Group one and locks everything
    /// already grouped. One per open group rather than one per page: a
    /// locked box is `disabled` and posts nothing, so a shared picker would
    /// make every save silently drop the members it was showing.
    pub picker: Vec<PickerDeviceCtx>,
}

// ---------------------------------------------------------------------
// Field-level exclusivity
// ---------------------------------------------------------------------

/// Who already holds each field of each entity, across a set of groups.
///
/// The unit of exclusivity is the **field**, not the entity
/// ([monitoring.md §4.2](../../plan/portal/monitoring.md)): a Node's
/// temperature can be in one group and its humidity in another. Built by
/// [`claims`] and consulted by exactly two callers — the picker, which grays
/// out what is taken, and `group_routes`' create/update, which refuses a POST
/// that would steal it. **One rule, two callers, so the two cannot
/// disagree**; a disabled checkbox posts nothing, but a hand-written POST is
/// under no obligation to respect it.
#[derive(Debug, Default, Clone)]
pub struct ClaimMap(std::collections::BTreeMap<String, EntityClaims>);

#[derive(Debug, Default, Clone)]
struct EntityClaims {
    /// The holder of a `fields = NULL` membership, which claims **every**
    /// field the entity reports — the dynamic meaning V8's `NULL` has.
    whole: Option<String>,
    /// Holders of individually named fields.
    fields: std::collections::BTreeMap<String, String>,
}

impl ClaimMap {
    /// The group holding `field` of `token`, if any. A whole-entity claim
    /// wins, because it covers every field by definition.
    pub fn holder(&self, token: &str, field: &str) -> Option<&str> {
        let e = self.0.get(token)?;
        e.whole
            .as_deref()
            .or_else(|| e.fields.get(field).map(String::as_str))
    }

    /// The group holding **all** of `token` — a `fields = NULL` membership.
    pub fn whole_holder(&self, token: &str) -> Option<&str> {
        self.0.get(token)?.whole.as_deref()
    }

    /// The group holding *anything* of `token`. This is what a submitted
    /// member with no field selection conflicts against: it claims every
    /// field, so any existing claim on this entity collides with it.
    pub fn any_holder(&self, token: &str) -> Option<&str> {
        let e = self.0.get(token)?;
        e.whole
            .as_deref()
            .or_else(|| e.fields.values().next().map(String::as_str))
    }

    /// The group that has this entity locked whole, given the fields the
    /// picker is offering for it. `None` while any field is still free.
    ///
    /// A wired probe offers no fields — it measures one thing — so any claim
    /// on it locks it, which is the pre-MB1 behaviour for that kind.
    fn entity_lock(&self, token: &str, offered: &[PickerFieldCtx]) -> Option<&str> {
        if offered.is_empty() {
            return self.any_holder(token);
        }
        if let Some(g) = self.whole_holder(token) {
            return Some(g);
        }
        let e = self.0.get(token)?;
        offered
            .iter()
            .all(|f| e.fields.contains_key(&f.name))
            .then(|| e.fields.get(&offered[0].name).map(String::as_str))
            .flatten()
    }
}

/// Reads every membership in `members` into a [`ClaimMap`], naming the group
/// from `groups`.
///
/// The conflict rule this supports needs **no live state** — no snapshot, no
/// "which fields is this Node reporting right now". A `NULL` membership
/// claims the whole entity and collides with anything else on it; two
/// explicit selections collide on their intersection. That is equivalent to
/// the spec's "NULL claims what the probe currently reports", because the
/// picker only ever offers fields the probe *does* report, so an explicit
/// selection is always a subset of them. A rule that read live state would
/// make a save's outcome depend on whether a Node happened to report in the
/// same second.
///
/// Members whose group is not in `groups` are skipped, and so is `exclude`.
/// Both callers pass the group being edited there, and they mean the same
/// thing by it: that group is about to replace its membership wholesale, so
/// its own rows must not read as somebody else's claim. `group_routes` needs
/// it or a group re-submitting its own fields would collide with itself; the
/// picker needs it or that group's members render locked in its own dialog —
/// checked, `disabled`, therefore never submitted, and the save drops them.
pub fn claims(
    groups: &[SensorGroup],
    members: &[SensorGroupMember],
    exclude: Option<i64>,
) -> ClaimMap {
    let mut map: std::collections::BTreeMap<String, EntityClaims> = Default::default();
    for m in members {
        if Some(m.group_id) == exclude {
            continue;
        }
        let Some(name) = groups.iter().find(|g| g.id == m.group_id).map(|g| &g.name) else {
            continue;
        };
        let Some(token) = member_token(m) else {
            continue;
        };
        let entry = map.entry(token).or_default();
        match m.fields.as_deref() {
            None | Some([]) => entry.whole = Some(name.clone()),
            Some(fields) => {
                for f in fields {
                    entry
                        .fields
                        .entry(f.clone())
                        .or_insert_with(|| name.clone());
                }
            }
        }
    }
    ClaimMap(map)
}

/// The picker key and the exclusivity key: `probe:{line}` / `node:{key}` /
/// `beacon:{mac}`.
///
/// **There is no hostname in it, and that is a recorded constraint rather
/// than an oversight.** Two Hubs' line 0 would produce the same
/// `probe:0`. Sensor groups are single-Hub throughout by
/// [decisions.md](../../plan/decisions.md) #8 — `group_routes::current_hostname`
/// stamps every member with the *first* device in the store, so a
/// multi-Hub install is already grouped wrong upstream of this token.
/// Widening the token here would fix nothing on its own and would silently
/// invalidate every stored membership. Multi-Hub grouping is phase-2 work
/// with its own design; when it arrives, this function, the stored
/// `hostname` column and the picker key change together.
fn member_token(m: &SensorGroupMember) -> Option<String> {
    if let Some(line) = m.sensor_line {
        return Some(format!("probe:{line}"));
    }
    if let Some(key) = &m.node_key {
        return Some(format!("node:{key}"));
    }
    m.mac.as_ref().map(|mac| format!("beacon:{mac}"))
}

/// One member's identity, decoded from a [`member_token`].
///
/// MB2's row fragment route is addressed by the token, so the token has to
/// travel *back* from the URL to the stored membership row it names. MB3's
/// probe-detail window is addressed the same way and reuses this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberRef {
    /// `i64`, not `u8`: this is the stored `sensor_groups_members.sensor_line`
    /// column's type, and the token has to compare equal to it.
    Probe(i64),
    Node(String),
    Beacon(String),
}

impl MemberRef {
    /// Does this stored membership row name the same entity?
    pub fn matches(&self, m: &SensorGroupMember) -> bool {
        match self {
            MemberRef::Probe(line) => m.sensor_line == Some(*line),
            MemberRef::Node(key) => m.node_key.as_deref() == Some(key.as_str()),
            MemberRef::Beacon(mac) => m.mac.as_deref() == Some(mac.as_str()),
        }
    }
}

/// The inverse of [`member_token`].
///
/// **Splits on the FIRST colon only**, for the same reason the group form
/// parser does: a Beacon's identity is a MAC and is full of them, so
/// `beacon:AA:BB:CC:DD:EE:01` decodes to one kind and one identity, not
/// seven fields. An unknown kind, an empty identity or a non-numeric probe
/// line is `None` — the caller turns that into "nothing to show" rather than
/// guessing.
pub fn parse_token(token: &str) -> Option<MemberRef> {
    let (kind, identity) = token.split_once(':')?;
    if identity.is_empty() {
        return None;
    }
    match kind {
        "probe" => identity.parse().ok().map(MemberRef::Probe),
        "node" => Some(MemberRef::Node(identity.to_string())),
        // MACs are stored uppercase (store ingest, group members); a
        // hand-typed `7c:d9:…` names the same Beacon.
        "beacon" => Some(MemberRef::Beacon(identity.trim().to_uppercase())),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// The SSE nudge's event names
// ---------------------------------------------------------------------

/// The row-nudge event names, one per entity kind.
///
/// **Entity-level, not row-level, and deliberately id-only.** The same
/// entity can sit in more than one group, and every browser watching it
/// chose its own of the four layouts — so the frame carries no markup at
/// all: it names an entity, and each row listening on that name refetches
/// itself from `/monitoring/groups/{id}/rows/{token}` in *that* session's
/// view mode. That inversion is what lets MB2 add no per-row `ChangeEvent`
/// variant (monitoring.backend.md §15).
///
/// These three are the single source for both ends of the contract: the row
/// builders stamp `GroupRowCtx::event` with them, and [`names_for_device`]
/// derives the same strings from a `ChangeEvent`. A name that only agreed by
/// coincidence would fail silently — the browser would simply never swap.
pub fn probe_event(hostname: &str, line: u8) -> String {
    format!("mon-probe-{hostname}-{line}")
}

pub fn node_event(hostname: &str, node_key: &str) -> String {
    format!("mon-node-{hostname}-{node_key}")
}

/// The MAC's colons are mangled to dashes exactly as the Beacon card's own
/// SSE id is (`beacon-{host}-{mac}`), so the two naming schemes stay
/// readable side by side in a devtools SSE pane.
pub fn beacon_event(hostname: &str, mac: &str) -> String {
    format!("mon-beacon-{hostname}-{}", mac.replace(':', "-"))
}

/// Every monitoring row nudge one [`ChangeEvent`] should produce.
///
/// Empty for every event that cannot move a monitoring row (device status,
/// config caches, command results): a nudge nobody listens to is a wasted
/// frame on every open tab.
///
/// The batch variants fan out to one name per entity in the snapshot, the
/// same way `sse.rs` already fans `NodeBatch` out to one card render each —
/// there is no combined row to send instead.
///
/// **Not gated on the displayed hostname.** The names are id-only and
/// [`sections`] renders members from whichever Hub stored them, so gating
/// here would silently stop repainting a row the page is showing.
/// Takes **one** device, never the fleet. The batch arms and `DeviceStatus`
/// are the only ones that need any state at all, and they need exactly the
/// Hub the event names; every other arm derives its names from the event's
/// own fields and ignores `device` entirely.
///
/// That shape is the point (review round 2): `sse.rs` calls this once per
/// open connection per reading, and a `&PortalState` parameter had it
/// cloning every device of every Hub on every `sensors/aggregated` message,
/// times every open tab, to read one device's key set.
///
/// `device` is `None` for a Hub the store no longer holds — no names, which
/// is what a fleet lookup that missed produced too.
pub fn names_for_device(change: &ChangeEvent, device: Option<&DeviceState>) -> Vec<String> {
    // HS-3 / R-8 (Hub sweep, review of !102): the rows listen under the store's spelling of
    // the Hub (`d.hostname`); an event may carry the topic's. Every arm names the event with
    // the store's spelling when the device is known.
    let canon = |hostname: &str| -> String {
        device
            .map(|d| d.hostname.as_str())
            .filter(|h| !h.is_empty())
            .unwrap_or(hostname)
            .to_string()
    };
    match change {
        ChangeEvent::SensorLine { hostname, line } => vec![probe_event(&canon(hostname), *line)],
        // Every row of a Hub is gated on its online state — wired probes
        // (UX-053), Nodes and Beacons (#30c) — so a Hub status change moves
        // all of them, and with them the headers carrying the banner.
        ChangeEvent::DeviceStatus { hostname, .. } => device
            .map(|d| {
                let host = canon(hostname);
                let probes = d.sensors.iter().map(|s| probe_event(&host, s.line));
                let nodes = d.nodes.keys().map(|k| node_event(&host, k));
                let beacons = d.ble_sensors.keys().map(|m| beacon_event(&host, m));
                probes.chain(nodes).chain(beacons).collect()
            })
            .unwrap_or_default(),
        ChangeEvent::Node { hostname, node_key } => vec![node_event(&canon(hostname), node_key)],
        ChangeEvent::NodeBatch { hostname } => device
            .map(|d| {
                let host = canon(hostname);
                d.nodes.keys().map(|k| node_event(&host, k)).collect()
            })
            .unwrap_or_default(),
        ChangeEvent::Beacon { hostname, mac } => vec![beacon_event(&canon(hostname), mac)],
        ChangeEvent::BeaconBatch { hostname } => device
            .map(|d| {
                let host = canon(hostname);
                d.ble_sensors
                    .keys()
                    .map(|mac| beacon_event(&host, mac))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The caller's stored `/monitoring` layout, validated.
///
/// A stored mode this build no longer renders (a row written before V8's
/// CHECK, or a layout since dropped) falls back rather than rendering
/// nothing. Factored out of `http::dashboard_handler` by MB2 because the two
/// fragment routes have to resolve it identically — a fragment rendered in
/// the wrong layout swaps a `<tr>` into a card grid.
pub fn effective_view_mode(user: &crate::auth::CurrentUser) -> &str {
    canonical_view_mode(&user.monitoring_view_mode).unwrap_or(DEFAULT_VIEW_MODE)
}

pub fn group_edit_ctxs(
    state: &PortalState,
    groups: &[SensorGroup],
    members: &[SensorGroupMember],
    liveness: &LivenessVerdicts,
) -> Vec<GroupEditCtx> {
    groups
        .iter()
        .map(|g| {
            let mine: Vec<_> = members.iter().filter(|m| m.group_id == g.id).collect();
            GroupEditCtx {
                picker: picker(state, groups, members, liveness, Some(g.id)),
                id: g.id,
                name: g.name.clone(),
                description: g.description.clone(),
                location: None,
                selected: mine.iter().filter_map(|m| member_token(m)).collect(),
                selected_fields: mine
                    .iter()
                    .flat_map(|m| {
                        let token = member_token(m);
                        m.fields
                            .iter()
                            .flatten()
                            .filter_map(move |f| token.as_ref().map(|t| format!("{t}|{f}")))
                            .collect::<Vec<_>>()
                    })
                    .collect(),
            }
        })
        .collect()
}

/// Everything tickable, per Hub. Discovered Beacon candidates are left out:
/// an unregistered tag has no readings to group.
///
/// `liveness` for the same reason the row builders take it: the green dot
/// beside a pickable Node or Beacon is the same claim the row makes, and
/// until review round 2 this was the last place in the monitoring surface
/// still reading it off the Hub's echo — so a Node the detector had measured
/// silent could be listed with a live dot beside the row that showed it
/// offline (devices.backend.md §19.1, F2/F3).
pub fn picker(
    state: &PortalState,
    groups: &[SensorGroup],
    members: &[SensorGroupMember],
    liveness: &LivenessVerdicts,
    exclude: Option<i64>,
) -> Vec<PickerDeviceCtx> {
    // The same map `group_routes` refuses a conflicting POST against, with
    // the same group excluded: the New Group dialog passes `None`, an edit
    // dialog passes its own id.
    let claimed = claims(groups, members, exclude);

    state
        .devices
        .values()
        .map(|device| PickerDeviceCtx {
            hostname: device.hostname.clone(),
            label: device_label(device),
            online: device.online,
            probes: device
                .sensors
                .iter()
                .map(|s| {
                    let token = format!("probe:{}", s.line);
                    PickerEntityCtx {
                        kind: "probe",
                        name: s.name.clone().unwrap_or_else(|| format!("Line {}", s.line)),
                        detail: None,
                        connected: s.connected,
                        // No fields to offer, so any claim locks it whole.
                        in_group: claimed.entity_lock(&token, &[]).map(str::to_string),
                        token,
                        fields: Vec::new(),
                        location: s.location.clone(),
                    }
                })
                .collect(),
            nodes: device
                .nodes
                .values()
                .map(|n| {
                    let token = format!("node:{}", n.node_key);
                    let fields = mark_claimed(node_selectable_fields(n), &claimed, &token);
                    PickerEntityCtx {
                        kind: "node",
                        name: n.name.clone().unwrap_or_else(|| n.node_key.clone()),
                        detail: Some(field_count_label(fields.len())),
                        connected: node_connected(
                            &normalize_radio_alarm_state(&n.alarm_state),
                            liveness.verdict(DeviceScope::Node, &n.node_key),
                        ),
                        in_group: claimed.entity_lock(&token, &fields).map(str::to_string),
                        token,
                        fields,
                        location: n.location.clone(),
                    }
                })
                .collect(),
            beacons: device
                .ble_sensors
                .values()
                .filter(|b| b.discovered != Some(true))
                .map(|b| {
                    let token = format!("beacon:{}", b.mac);
                    let fields: Vec<_> = fiber_protocol::eye_registry()
                        .iter()
                        .filter(|f| f.thresholdable && f.name != "battery")
                        .map(|f| PickerFieldCtx {
                            name: f.name.to_string(),
                            label: f.label.to_string(),
                            in_group: None,
                        })
                        .collect();
                    let fields = mark_claimed(fields, &claimed, &token);
                    PickerEntityCtx {
                        kind: "beacon",
                        name: b.name.clone().unwrap_or_else(|| b.mac.clone()),
                        detail: Some(field_count_label(fields.len())),
                        connected: node_connected(
                            &normalize_radio_alarm_state(&b.alarm_state),
                            liveness.verdict(DeviceScope::Beacon, &b.mac),
                        ),
                        in_group: claimed.entity_lock(&token, &fields).map(str::to_string),
                        token,
                        fields,
                        location: b.location.clone(),
                    }
                })
                .collect(),
        })
        .collect()
}

/// How many quantities a Node or Beacon offers, in the request's language:
/// `"2 čidla"`, or "none yet" for hardware that has not reported a
/// thresholdable reading. The plural buckets are the group header's.
fn field_count_label(count: usize) -> String {
    let key = match count {
        0 => "monitoring.groups.field_count_none",
        1 => "monitoring.groups.field_count_one",
        2..=4 => "monitoring.groups.field_count_few",
        _ => "monitoring.groups.field_count_many",
    };
    crate::i18n::lookup(crate::i18n::current_lang(), key).replace("{n}", &count.to_string())
}

/// A Node offers the fields it is actually reporting, not all 25 in the
/// registry — a checkbox for a field this hardware never sends is noise.
fn node_selectable_fields(n: &NodeState) -> Vec<PickerFieldCtx> {
    fiber_protocol::lorawan_registry()
        .iter()
        .filter(|f| {
            f.kind == fiber_protocol::LorawanFieldKind::Continuous
                && f.thresholdable
                && f.group != fiber_protocol::LorawanFieldGroup::Battery
                && n.fields.contains_key(f.name)
        })
        .map(|f| PickerFieldCtx {
            name: f.name.to_string(),
            label: f.label.to_string(),
            in_group: None,
        })
        .collect()
}

/// Stamps each offered field with the group that already holds it.
fn mark_claimed(
    mut fields: Vec<PickerFieldCtx>,
    claimed: &ClaimMap,
    token: &str,
) -> Vec<PickerFieldCtx> {
    for f in fields.iter_mut() {
        f.in_group = claimed.holder(token, &f.name).map(str::to_string);
    }
    fields
}

// ---------------------------------------------------------------------
// Sparklines
// ---------------------------------------------------------------------

/// Fills every card's trend line from the last 24 h of stored history.
///
/// Only the detailed CARD layout draws one, so only that layout pays for it —
/// one query per card, which on a Hub is a local indexed SQLite read. v1
/// spends one HTTP round-trip per card for the same picture; this is the same
/// shape of cost without the network.
///
/// A query that fails leaves the card's `spark` as `None` and the card draws
/// its empty trend placeholder. A missing trend line is not worth failing a
/// page over.
///
/// Identity comes off `GroupRowCtx::token`, not off the DOM id. It used to
/// parse `row_id` — the line from its last dash-segment, the MAC by trimming
/// a prefix — and MB1's group-id prefix broke the Beacon case silently,
/// because a `trim_start_matches` that does not match is a no-op. A DOM id is
/// a presentation detail; the token is the identity.
pub async fn fill_sparklines(sections: &mut [GroupSectionCtx], db: &crate::db::Db, now: i64) {
    for section in sections.iter_mut() {
        for row in section.rows.iter_mut() {
            fill_row_sparkline(row, db, now).await;
        }
    }
}

/// The same trend fill for exactly one row — what
/// `GET /monitoring/groups/{id}/rows/{token}` needs, since a swapped-in
/// `card-detailed` row without its sparkline would blank the graph the page
/// had drawn a moment earlier.
///
/// Split out of [`fill_sparklines`] rather than duplicated: the two paths
/// must query the same window and read identity off the same field.
pub async fn fill_row_sparkline(row: &mut GroupRowCtx, db: &crate::db::Db, now: i64) {
    // 24 h: the card prints this window's min and max (variant B, 2026-10-15).
    let from = now - 86_400;
    let hostname = row.hostname.clone();
    let row_id = row.row_id.clone();
    let kind = row.kind;
    let token = row.token.clone();
    for field in row.fields.iter_mut() {
        let values = match (kind, field.field.as_deref()) {
            ("probe", _) => {
                let Some(line) = token.strip_prefix("probe:").and_then(|s| s.parse().ok()) else {
                    continue;
                };
                let h = hostname.clone();
                db.run(move |s| s.query_minutes(&h, line, from, now))
                    .await
                    .map(|rows| rows.iter().filter_map(|r| r.avg_c).collect::<Vec<_>>())
            }
            ("node", Some(name)) => {
                let Some(key) = token.strip_prefix("node:") else {
                    continue;
                };
                let (h, key, name) = (hostname.clone(), key.to_string(), name.to_string());
                // The card's value is in the displayed unit, so is its trend.
                let f = fiber_protocol::lorawan_field(&name);
                db.run(move |s| s.query_field_readings(&h, &key, &name, from, now))
                    .await
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|r| r.value)
                            .map(|v| f.map_or(v, |f| f.to_display(v)))
                            .collect::<Vec<_>>()
                    })
            }
            ("beacon", Some(name)) => {
                let Some(mac) = token.strip_prefix("beacon:") else {
                    continue;
                };
                let (h, mac) = (hostname.clone(), mac.to_string());
                db.run(move |s| s.query_eye_readings(&h, &mac, from, now, None))
                    .await
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|r| match name {
                                "temperature" => r.temperature_c,
                                "humidity" => r.humidity_pct.map(|h| h as f64),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                    })
            }
            _ => continue,
        };
        match values {
            Ok(values) => field.spark = SparkCtx::from_values(&values),
            Err(err) => {
                tracing::warn!(%err, row = %row_id, "sparkline history query failed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `table-simplified` is gone (2026-10-15): a stored or posted value of
    /// it lands on the detailed table, never on a 400 or a blank page.
    #[test]
    fn the_removed_simple_table_falls_back_to_the_detailed_one() {
        assert!(!is_view_mode("table-simplified"));
        assert_eq!(
            canonical_view_mode("table-simplified"),
            Some("table-detailed")
        );
        assert_eq!(
            canonical_view_mode("card-simplified"),
            Some("card-simplified")
        );
        assert_eq!(canonical_view_mode("bogus"), None);
        // The default is Overview (cards detailed), Hynek 2026-10-15.
        assert_eq!(DEFAULT_VIEW_MODE, "card-detailed");
    }

    // ---- Field exclusivity and header aggregates (MB1, §4) -----------

    fn group(id: i64, name: &str) -> SensorGroup {
        SensorGroup {
            id,
            name: name.into(),
            description: None,
            location: None,
            sort_order: 0,
            created_at: "2026-09-03".into(),
        }
    }

    /// A Node membership. `fields: None` is the "all fields" NULL.
    fn node_member(group_id: i64, key: &str, fields: Option<&[&str]>) -> SensorGroupMember {
        SensorGroupMember {
            id: 0,
            group_id,
            hostname: "hub".into(),
            sensor_line: None,
            node_key: Some(key.into()),
            mac: None,
            fields: fields.map(|f| f.iter().map(|s| (*s).to_string()).collect()),
            sort_order: 0,
        }
    }

    fn probe_member(group_id: i64, line: i64) -> SensorGroupMember {
        SensorGroupMember {
            sensor_line: Some(line),
            node_key: None,
            ..node_member(group_id, "unused", None)
        }
    }

    fn beacon_member(group_id: i64, mac: &str) -> SensorGroupMember {
        SensorGroupMember {
            node_key: None,
            mac: Some(mac.into()),
            ..node_member(group_id, "unused", None)
        }
    }

    /// The edit picker drops the edited group's own rows, so they are not
    /// named as held by another group.
    #[test]
    fn a_group_is_not_named_as_its_own_holder() {
        let groups = [group(1, "A")];
        let members = [node_member(1, "n1", Some(&["temperature"]))];

        let all = claims(&groups, &members, None);
        assert_eq!(all.holder("node:n1", "temperature"), Some("A"));

        // What the edit page's picker passes: its own rows name no holder.
        let excluding_self = claims(&groups, &members, Some(1));
        assert_eq!(excluding_self.holder("node:n1", "temperature"), None);
    }

    fn state_with_one_node(fields: &[(&str, f64)]) -> PortalState {
        let mut device = DeviceState::new("hub".into());
        device.online = true;
        let mut n = NodeState::new("n1".into(), 0);
        n.alarm_state = "NORMAL".into();
        for (name, value) in fields {
            n.fields.insert((*name).to_string(), *value);
            n.field_alarm_states
                .insert((*name).to_string(), "NORMAL".to_string());
        }
        device.nodes.insert("n1".into(), n);
        let mut state = PortalState::default();
        state.devices.insert("hub".into(), device);
        state
    }

    /// UI-M4: one field taken leaves the rest of the entity selectable, and
    /// the claimed box names the group holding it.
    #[test]
    fn the_picker_locks_a_claimed_field_and_leaves_its_neighbour_free() {
        let state = state_with_one_node(&[("temperature", 4.0), ("humidity", 50.0)]);
        let groups = [group(1, "Alpha")];

        let one_taken = picker(
            &state,
            &groups,
            &[node_member(1, "n1", Some(&["temperature"]))],
            &LivenessVerdicts::default(),
            None,
        );
        let node = &one_taken[0].nodes[0];
        assert_eq!(node.in_group, None, "the entity is still tickable");
        let by_name = |n: &str| -> Option<String> {
            node.fields
                .iter()
                .find(|f| f.name == n)
                .and_then(|f| f.in_group.clone())
        };
        assert_eq!(by_name("temperature").as_deref(), Some("Alpha"));
        assert_eq!(by_name("humidity"), None, "the free field stays free");

        // Every offered field taken -> the whole row locks, today's visual.
        let both_taken = picker(
            &state,
            &groups,
            &[node_member(1, "n1", Some(&["temperature", "humidity"]))],
            &LivenessVerdicts::default(),
            None,
        );
        assert_eq!(both_taken[0].nodes[0].in_group.as_deref(), Some("Alpha"));

        // So does a NULL membership, which claims whatever it reports.
        let null_claim = picker(
            &state,
            &groups,
            &[node_member(1, "n1", None)],
            &LivenessVerdicts::default(),
            None,
        );
        assert_eq!(null_claim[0].nodes[0].in_group.as_deref(), Some("Alpha"));
    }

    /// The edit dialog's own picker must not lock the group it belongs to.
    /// A locked box renders `disabled`, a disabled box posts nothing, and
    /// membership is replaced wholesale — so a shared picker made every save
    /// from an edit dialog drop the members it was showing as ticked.
    #[test]
    fn an_edit_dialogs_picker_leaves_its_own_group_selectable() {
        let state = state_with_one_node(&[("temperature", 4.0), ("humidity", 50.0)]);
        let groups = [group(1, "Alpha"), group(2, "Beta")];
        let members = [node_member(1, "n1", None), probe_member(2, 0)];

        let mine = group_edit_ctxs(&state, &groups, &members, &LivenessVerdicts::default());
        let alpha = mine.iter().find(|g| g.id == 1).expect("Alpha");
        let node = &alpha.picker[0].nodes[0];
        assert_eq!(node.in_group, None, "its own member stays tickable");
        assert!(
            alpha.selected.contains(&"node:n1".to_string()),
            "and stays ticked"
        );
        let probe = alpha.picker[0]
            .probes
            .iter()
            .find(|p| p.token == "probe:0")
            .expect("line 0");
        assert_eq!(
            probe.in_group.as_deref(),
            Some("Beta"),
            "another group's claim still locks"
        );
    }

    /// A wired probe offers no fields — it measures one thing — so any claim
    /// locks it whole, exactly as before MB1.
    #[test]
    fn a_wired_probe_has_no_fields_to_split_and_locks_whole() {
        let mut device = DeviceState::new("hub".into());
        device.sensors[0].connected = true;
        let mut state = PortalState::default();
        state.devices.insert("hub".into(), device);

        let member = SensorGroupMember {
            id: 0,
            group_id: 1,
            hostname: "hub".into(),
            sensor_line: Some(0),
            node_key: None,
            mac: None,
            fields: None,
            sort_order: 0,
        };
        let entries = picker(
            &state,
            &[group(1, "Alpha")],
            &[member],
            &LivenessVerdicts::default(),
            None,
        );
        let probe = entries[0].probes.iter().find(|p| p.token == "probe:0");
        assert_eq!(probe.expect("line 0").in_group.as_deref(), Some("Alpha"));
    }

    /// §3.1: "skupina se 3 Nody à T+H = 6 čidel" — the header counts value
    /// cards, not probes.
    #[test]
    fn a_sections_header_counts_fields_not_probes() {
        let mut device = DeviceState::new("hub".into());
        device.online = true;
        for key in ["n1", "n2", "n3"] {
            let mut n = NodeState::new(key.into(), 0);
            n.alarm_state = "NORMAL".into();
            for field in ["temperature", "humidity"] {
                n.fields.insert(field.into(), 4.0);
                n.field_alarm_states.insert(field.into(), "NORMAL".into());
            }
            device.nodes.insert(key.into(), n);
        }
        let mut state = PortalState::default();
        state.devices.insert("hub".into(), device);

        let members: Vec<_> = ["n1", "n2", "n3"]
            .iter()
            .map(|k| node_member(1, k, None))
            .collect();
        let sections = sections(
            &state,
            &[group(1, "Alpha")],
            &members,
            &Default::default(),
            &Default::default(),
        );

        assert_eq!(sections[0].rows.len(), 3, "three rows");
        assert_eq!(sections[0].field_count, 6, "six cards");
        assert_eq!(sections[0].alarm_state, "Normal");
        assert_eq!(sections[0].out_of_range, 0);
        assert_eq!(sections[0].fault_count, 0, "MB2 fills this");
    }

    /// monitoring.md §2.1 (amended 2026-09-25): the strip counts the probes the
    /// groups show — a Node in two groups once, an ungrouped one not at all,
    /// however alarming.
    #[test]
    fn the_monitoring_strip_counts_grouped_probes_once() {
        let mut device = DeviceState::new("hub".into());
        device.online = true;
        for (key, state) in [("n1", "NORMAL"), ("n2", "WARNING"), ("n3", "CRITICAL")] {
            let mut n = NodeState::new(key.into(), 0);
            n.alarm_state = state.into();
            for field in ["temperature", "humidity"] {
                n.fields.insert(field.into(), 4.0);
                n.field_alarm_states.insert(field.into(), state.into());
            }
            device.nodes.insert(key.into(), n);
        }
        let mut state = PortalState::default();
        state.devices.insert("hub".into(), device);

        let members = [
            node_member(1, "n1", Some(&["temperature"])),
            node_member(2, "n1", Some(&["humidity"])),
            node_member(1, "n2", None),
        ];
        let sections = sections(
            &state,
            &[group(1, "Alpha"), group(2, "Beta")],
            &members,
            &Default::default(),
            &Default::default(),
        );
        let stats = crate::render::monitoring_stats_ctx(true, &sections);
        let v: Vec<i64> = stats.tiles.iter().map(|t| t.value).collect();
        // total: n1 once and n2; out of range: n2 only — n3 is critical but
        // in no group, so the page never shows it and the strip must not
        // count it either.
        assert_eq!(v, [2, 2, 1, 0], "total, online, out of range, faults");
    }

    /// UX-053: a wired probe has no voice but its Hub's, so an offline Hub's
    /// probes read disconnected and drop out of the strip's "Online", while
    /// still carrying no fault of their own (monitoring.md §3.1: the Hub's
    /// `DeviceOffline` is the one alarm, monitoring.backend.md WP2.2).
    #[test]
    fn an_offline_hubs_wired_probe_is_not_connected() {
        let mut device = DeviceState::new("hub".into());
        device.online = false;
        device.sensors[0].connected = true;
        device.sensors[0].temperature_avg_celsius = Some(4.0);
        let mut state = PortalState::default();
        state.devices.insert("hub".into(), device);

        let sections = sections(
            &state,
            &[group(1, "Alpha")],
            &[probe_member(1, 0)],
            &Default::default(),
            &Default::default(),
        );
        let row = &sections[0].rows[0];
        assert!(!row.connected, "the Hub is the probe's only voice");
        assert!(row.faults.is_empty(), "no Porucha of its own");
        let stats = crate::render::monitoring_stats_ctx(true, &sections);
        let v: Vec<i64> = stats.tiles.iter().map(|t| t.value).collect();
        assert_eq!(v, [1, 0, 0, 0], "total, online, out of range, faults");
    }

    /// Decision 2026-09-26: the wired "Alarm" pair is a v1 leftover (the
    /// device's defaults 0 / 100 °C disable it), so it alone is no limit to
    /// be "normal" against.
    #[test]
    fn the_alarm_pair_alone_is_not_a_limit() {
        let s = SensorState {
            line: 0,
            connected: true,
            temperature_avg_celsius: Some(4.0),
            thresholds: Some(crate::state::SensorThresholds {
                alarm_low: Some(0.0),
                alarm_high: Some(100.0),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!probe_row(1, "hub", "Hub", &s, true).has_limits);
    }

    /// UX-038: a Monitoring card shows a Node's pressure in hPa, its gauge
    /// bounds in the same unit. The v2 decoder sends hPa, shown as sent.
    #[test]
    fn a_nodes_pressure_card_reads_in_hpa() {
        let mut state = state_with_one_node(&[("pressure", 1013.288)]);
        state
            .devices
            .get_mut("hub")
            .unwrap()
            .nodes
            .get_mut("n1")
            .unwrap()
            .field_thresholds = Some(vec![crate::state::NodeFieldThreshold {
            field: "pressure".into(),
            critical_low: Some(990.0),
            warning_low: None,
            warning_high: None,
            critical_high: Some(1035.0),
        }]);
        let sections = sections(
            &state,
            &[group(1, "Alpha")],
            &[node_member(1, "n1", None)],
            &Default::default(),
            &Default::default(),
        );
        let f = &sections[0].rows[0].fields[0];
        assert_eq!(f.unit, "hPa");
        assert!((f.value.unwrap() - 1013.288).abs() < 1e-9);
        let t = f.thresholds.as_ref().unwrap();
        assert_eq!(
            (t.critical_low, t.critical_high),
            (Some(990.0), Some(1035.0))
        );
    }

    /// #30c: Nodes and Beacons behind an offline Hub read "neznámý" at once
    /// instead of waiting for their own offline detector. Not connected, no
    /// fault of their own (the group's banner names the Hub), and the group
    /// lists the Hub with the time it was last heard.
    #[test]
    fn an_offline_hubs_nodes_and_beacons_read_unknown_under_a_banner() {
        let mut state = state_with_one_node(&[("temperature", 4.0)]);
        let device = state.devices.get_mut("hub").unwrap();
        device.online = false;
        device.last_seen = Some(1_790_000_000);
        device.system.device_label = Some("Sklad".into());
        let mut tag = beacon_with("NORMAL", &[("temperature", "NORMAL")]);
        tag.discovered = None;
        tag.low_battery = Some(true);
        device.ble_sensors.insert(tag.mac.clone(), tag);
        // The detector already says the Node is silent: still no fault of
        // its own, the Hub's banner covers it.
        let verdicts = liveness([(DeviceScope::Node, "n1", true)]);

        let sections = sections(
            &state,
            &[group(1, "Alpha")],
            &[
                node_member(1, "n1", None),
                beacon_member(1, "AA:BB:CC:DD:EE:FF"),
            ],
            &Default::default(),
            &verdicts,
        );
        let s = &sections[0];
        assert_eq!(s.rows.len(), 2);
        for row in &s.rows {
            assert!(row.unknown, "{} reads unknown", row.kind);
            assert!(!row.connected, "{} is not online", row.kind);
            assert!(row.faults.is_empty(), "{} has no fault", row.kind);
        }
        assert_eq!(s.fault_count, 0);
        assert_eq!(s.offline_hubs.len(), 1, "one line per offline Hub");
        assert_eq!(s.offline_hubs[0].name, "Sklad");
        assert_eq!(
            s.offline_hubs[0].since.as_deref(),
            unix_to_rfc3339(1_790_000_000).as_deref()
        );
        let stats = crate::render::monitoring_stats_ctx(true, &sections);
        let v: Vec<i64> = stats.tiles.iter().map(|t| t.value).collect();
        assert_eq!(v, [2, 0, 0, 0], "total, online, out of range, faults");

        // An online Hub: no banner, rows are what their own verdicts say.
        state.devices.get_mut("hub").unwrap().online = true;
        let sections = super::sections(
            &state,
            &[group(1, "Alpha")],
            &[node_member(1, "n1", None)],
            &Default::default(),
            &Default::default(),
        );
        assert!(sections[0].offline_hubs.is_empty());
        assert!(!sections[0].rows[0].unknown);
        assert!(sections[0].rows[0].connected);
    }

    /// The store keys `devices` by the ASCII-lowercased hostname while the
    /// membership row holds the wire spelling the Hub publishes under
    /// (`FIBER-MOCK-001`). Resolving the member through `PortalState::device`
    /// is what bridges the two; indexing the map directly dropped every
    /// member of every group and rendered the section empty.
    #[test]
    fn a_member_stamped_with_the_wire_hostname_still_resolves() {
        let mut device = DeviceState::new("FIBER-MOCK-001".into());
        device.online = true;
        let mut n = NodeState::new("n1".into(), 0);
        n.alarm_state = "NORMAL".into();
        n.fields.insert("temperature".into(), 4.0);
        n.field_alarm_states
            .insert("temperature".into(), "NORMAL".into());
        device.nodes.insert("n1".into(), n);
        let mut state = PortalState::default();
        state.devices.insert("fiber-mock-001".into(), device);

        let member = SensorGroupMember {
            hostname: "FIBER-MOCK-001".into(),
            ..node_member(1, "n1", None)
        };
        let sections = sections(
            &state,
            &[group(1, "Alpha")],
            &[member],
            &Default::default(),
            &Default::default(),
        );

        assert_eq!(sections[0].rows.len(), 1, "the member resolves");
        assert_eq!(sections[0].field_count, 1);
    }

    /// The worst value class wins the header, and only Warning/Critical
    /// fields are counted as out of range.
    #[test]
    fn a_section_reports_its_worst_field_and_how_many_are_out_of_range() {
        let mut state = state_with_one_node(&[("temperature", 4.0), ("humidity", 50.0)]);
        {
            let n = state
                .devices
                .get_mut("hub")
                .unwrap()
                .nodes
                .get_mut("n1")
                .unwrap();
            n.field_alarm_states
                .insert("temperature".into(), "CRITICAL".into());
        }
        let sections = sections(
            &state,
            &[group(1, "Alpha")],
            &[node_member(1, "n1", None)],
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(sections[0].field_count, 2);
        assert_eq!(sections[0].alarm_state, "Critical");
        assert_eq!(sections[0].out_of_range, 1, "humidity is Normal");
    }

    /// The reason `row_id` carries the group id: after per-field exclusivity
    /// one Node legitimately renders in two sections, and htmx swaps by id.
    #[test]
    fn the_same_node_in_two_groups_gets_two_distinct_row_ids() {
        let state = state_with_one_node(&[("temperature", 4.0), ("humidity", 50.0)]);
        let sections = sections(
            &state,
            &[group(1, "Alpha"), group(2, "Beta")],
            &[
                node_member(1, "n1", Some(&["temperature"])),
                node_member(2, "n1", Some(&["humidity"])),
            ],
            &Default::default(),
            &Default::default(),
        );

        assert_ne!(sections[0].rows[0].row_id, sections[1].rows[0].row_id);
        assert_eq!(sections[0].rows[0].row_id, "row-1-node-hub-n1");
        assert_eq!(sections[1].rows[0].row_id, "row-2-node-hub-n1");

        // And each section shows only the field it holds.
        let fields = |s: &GroupSectionCtx| -> Vec<String> {
            s.rows[0]
                .fields
                .iter()
                .filter_map(|f| f.field.clone())
                .collect()
        };
        assert_eq!(fields(&sections[0]), ["temperature"]);
        assert_eq!(fields(&sections[1]), ["humidity"]);
        assert_eq!(sections[0].field_count, 1);
        assert_eq!(sections[1].field_count, 1);
    }

    // ---- Value class vs connectivity (MB0, §3 WP0.2) -----------------

    fn node_with(alarm_state: &str, field_states: &[(&str, &str)]) -> NodeState {
        let mut n = NodeState::new("aabbccddeeff0011".to_string(), 0);
        n.alarm_state = alarm_state.to_string();
        for (field, state) in field_states {
            n.fields.insert((*field).to_string(), 21.0);
            n.field_alarm_states
                .insert((*field).to_string(), (*state).to_string());
        }
        n
    }

    fn beacon_with(alarm_state: &str, field_states: &[(&str, &str)]) -> BeaconState {
        let mut b = BeaconState::new("AA:BB:CC:DD:EE:FF".to_string(), 0);
        b.alarm_state = alarm_state.to_string();
        for (field, state) in field_states {
            match *field {
                "temperature" => b.temperature = Some(4.0),
                "humidity" => b.humidity = Some(50),
                other => panic!("no such beacon reading: {other}"),
            }
            b.field_alarm_states
                .insert((*field).to_string(), (*state).to_string());
        }
        b
    }

    /// The three connectivity states now rank below `Normal`, so they can
    /// never win a row's value class.
    #[test]
    fn worst_ranks_the_value_states_only() {
        let cases: &[(&[&str], Option<&str>)] = &[
            (&[], None),
            (&["Normal"], None),
            (&["Disconnected"], None),
            (&["NeverConnected", "Reconnecting"], None),
            (&["Normal", "Warning"], Some("Warning")),
            (&["Warning", "Critical"], Some("Critical")),
            // The case the split exists for: a silent entity whose last
            // reading was out of range is Warning, not Disconnected.
            (&["Disconnected", "Warning"], Some("Warning")),
        ];
        for (states, expected) in cases {
            let owned: Vec<String> = states.iter().map(|s| (*s).to_string()).collect();
            assert_eq!(worst(&owned).as_deref(), *expected, "worst({states:?})");
        }
    }

    #[test]
    fn a_silent_node_still_reports_the_class_of_its_last_reading() {
        let row = node_row(
            1,
            "hub",
            "Hub",
            &node_with("DISCONNECTED", &[("temperature", "WARNING")]),
            None,
            &Default::default(),
        );
        assert_eq!(row.alarm_state, "Warning", "the value axis");
        assert!(!row.connected, "the connectivity axis, separately");
    }

    /// "Renders `Normal` rather than borrowing its entity state" — §3's
    /// *Accept:* verbatim. Before MB0 this row read `Disconnected`.
    #[test]
    fn a_node_with_no_cards_is_normal_not_its_entity_state() {
        let row = node_row(
            1,
            "hub",
            "Hub",
            &node_with("DISCONNECTED", &[]),
            None,
            &Default::default(),
        );
        assert_eq!(row.alarm_state, "Normal");
        assert!(!row.connected);
    }

    #[test]
    fn a_beacon_splits_the_two_axes_the_same_way() {
        let warm = beacon_row(
            1,
            "hub",
            "Hub",
            &beacon_with("DISCONNECTED", &[("temperature", "CRITICAL")]),
            &[],
            None,
            &Default::default(),
        );
        assert_eq!(warm.alarm_state, "Critical");
        assert!(!warm.connected);

        let silent = beacon_row(
            1,
            "hub",
            "Hub",
            &beacon_with("NEVERCONNECTED", &[]),
            &[],
            None,
            &Default::default(),
        );
        assert_eq!(silent.alarm_state, "Normal");
        assert!(!silent.connected);
    }

    /// A wired probe carries `connected` on the model already, so its
    /// `dominant_alarm_state` contributes only its value half — to the row
    /// and to its single card alike.
    #[test]
    fn a_probes_connectivity_state_never_reaches_its_value_class() {
        let s = SensorState {
            line: 3,
            dominant_alarm_state: "DISCONNECTED".to_string(),
            connected: false,
            temperature_avg_celsius: Some(-18.0),
            ..Default::default()
        };
        let row = probe_row(1, "hub", "Hub", &s, true);
        assert_eq!(row.alarm_state, "Normal");
        assert_eq!(row.fields[0].alarm_state, "Normal");
        assert!(!row.connected, "still read from the model's own flag");

        let alarming = SensorState {
            dominant_alarm_state: "WARNING".to_string(),
            connected: true,
            ..s.clone()
        };
        let row = probe_row(1, "hub", "Hub", &alarming, true);
        assert_eq!(row.alarm_state, "Warning");
        assert!(row.connected);
    }

    // ---- MB2: one liveness verdict, faults, tokens, event names -------

    fn liveness<'a>(v: impl IntoIterator<Item = (DeviceScope, &'a str, bool)>) -> LivenessVerdicts {
        LivenessVerdicts::from_verdicts(v)
    }

    /// The F2/F3 residue closed for the monitoring page: `connected` on a
    /// group row is the **detector's** verdict, and the Hub's echoed
    /// `alarm_state` survives only where the detector holds none.
    ///
    /// Both directions matter. A Node silent past its budget keeps echoing
    /// `NORMAL` (F2) — the case that put B2's fault triangle beside a green
    /// dot; a Node fed only by the v2 per-Node envelope is never written and
    /// stays `NEVERCONNECTED` however recently it reported (F3). The dev
    /// mock publishes the v1 aggregate, so neither is visible in the dev
    /// loop and this test is what holds the rule.
    #[test]
    fn the_detector_overrides_the_hubs_echo_on_a_group_row() {
        let key = "aabbccddeeff0011";

        let silent_but_echoing_normal = node_row(
            1,
            "hub",
            "Hub",
            &node_with("NORMAL", &[("temperature", "NORMAL")]),
            None,
            &liveness([(DeviceScope::Node, key, true)]),
        );
        assert!(
            !silent_but_echoing_normal.connected,
            "the detector says silent, so the row is not connected"
        );
        assert_eq!(
            silent_but_echoing_normal.faults,
            ["offline"],
            "and the same verdict draws the fault triangle"
        );

        let reporting_but_echoing_never = node_row(
            1,
            "hub",
            "Hub",
            &node_with("NEVERCONNECTED", &[("temperature", "NORMAL")]),
            None,
            &liveness([(DeviceScope::Node, key, false)]),
        );
        assert!(
            reporting_but_echoing_never.connected,
            "a verdict of online beats a stale NEVERCONNECTED echo"
        );
        assert!(reporting_but_echoing_never.faults.is_empty());

        // No verdict at all — before the first sweep, or with detection off
        // — is the one case the echo still answers, and it is not a fault.
        let no_verdict = node_row(
            1,
            "hub",
            "Hub",
            &node_with("NEVERCONNECTED", &[]),
            None,
            &Default::default(),
        );
        assert!(!no_verdict.connected, "the echo answers");
        assert!(
            no_verdict.faults.is_empty(),
            "waiting for join is a lifecycle state, not a silence"
        );
    }

    /// One battery unit across the whole portal (devices.beacon.md §2.3).
    ///
    /// The Beacon card and the detail page have always read volts out of
    /// `render::beacon_ctx`; this row printed the wire's raw millivolts, so
    /// the same Beacon read "2.90 V" on one page and "2904 mV" on the
    /// dashboard beside a Node's "3.60 V". Both go through
    /// `render::beacon_battery_volts` now.
    #[test]
    fn a_beacon_row_reports_its_battery_in_volts_like_every_other_row() {
        let mut b = beacon_with("NORMAL", &[("temperature", "NORMAL")]);
        b.battery_mv = Some(2904);
        let row = beacon_row(1, "hub", "Hub", &b, &[], None, &Default::default());
        assert_eq!(row.battery.as_deref(), Some("2.90 V"));
        assert_eq!(
            row.battery,
            crate::render::beacon_ctx(crate::render::BeaconCardInputs {
                beacon: &b,
                meta: None,
                thresholds: &[],
                offline: None,
                signal_bands: &Default::default(),
                hubs: Vec::new(),
                calibration: None,
            })
            .battery,
            "the row and the card are formatted by one helper"
        );

        let mut unknown = beacon_with("NORMAL", &[("temperature", "NORMAL")]);
        unknown.battery_mv = None;
        assert!(
            beacon_row(1, "hub", "Hub", &unknown, &[], None, &Default::default())
                .battery
                .is_none(),
            "no reading is a blank cell, not '0.00 V'"
        );
    }

    /// Variant B (monitoring.md, amendment 2026-10-15): the detailed card
    /// carries the data age in its header and drops the old footer — no
    /// interval placeholders, no "All normal" caption.
    #[test]
    fn the_detailed_card_shows_data_age_and_no_footer() {
        let b = beacon_with("NORMAL", &[("temperature", "NORMAL")]);
        let mut row = beacon_row(1, "hub", "Hub", &b, &[], None, &Default::default());
        row.last_seen_short = Some("40 s".into());
        row.last_seen_unix = Some(1);
        let html = crate::render::Templates::new("templates", "static")
            .env()
            .get_template("partials/group_row.html")
            .unwrap()
            .render(minijinja::context! {
                lang => "cs", r => row, view_cards => true, view_simple => false,
            })
            .unwrap();
        assert!(html.contains(">40 s<"), "{html}");
        assert!(html.contains("data-age-ts=\"1\""), "{html}");
        assert!(!html.contains(r#"title="Interval měření""#), "{html}");
        assert!(!html.contains("Vše v normálu"), "{html}");
    }

    /// UX-055: a field switched on with every bound cleared has nothing to be
    /// normal against, so the card says "no limit set", not a green
    /// "All normal", and the table row's badge says the same.
    #[test]
    fn an_enabled_field_without_bounds_is_not_all_normal() {
        let b = beacon_with("NORMAL", &[("temperature", "NORMAL")]);
        let limit = |warning_high: Option<f64>| FieldThreshold {
            hostname: "hub".into(),
            scope: portal_storage::THRESHOLD_SCOPE_EYE.into(),
            entity: b.mac.clone(),
            field: "temperature".into(),
            critical_low: None,
            warning_low: None,
            warning_high,
            critical_high: None,
            enabled: true,
            updated_at: None,
            hysteresis: None,
        };
        let cleared = beacon_row(
            1,
            "hub",
            "Hub",
            &b,
            &[limit(None)],
            None,
            &Default::default(),
        );
        let bounded = beacon_row(
            1,
            "hub",
            "Hub",
            &b,
            &[limit(Some(8.0))],
            None,
            &Default::default(),
        );
        assert!(!cleared.has_limits);
        assert!(bounded.has_limits);

        let templates = crate::render::Templates::new("templates", "static");
        let render = |r: &GroupRowCtx, view_cards: bool| {
            templates
                .env()
                .get_template("partials/group_row.html")
                .unwrap()
                .render(minijinja::context! {
                    lang => "en", r => r, view_cards => view_cards, view_simple => false,
                })
                .unwrap()
        };
        // Variant B cards carry no verdict caption at all (2026-10-15); the
        // grey limits bar says "no limit" there.
        let card = render(&cleared, true);
        assert!(!card.contains("All normal"), "{card}");
        assert!(card.contains("bg-muted"), "the empty limits bar: {card}");
        // The detailed table (variant A, 2026-10-15) has no verdict text
        // either: the grey limits bar, and no green state badge.
        let row = render(&cleared, false);
        assert!(
            row.contains("bg-muted") && !row.contains(">Normal<"),
            "{row}"
        );

        // Since 2026-10-15 Normal is green with or without limits in every
        // layout: the cards' tint, and both tables' state stripe (variant A).
        let render_in = |r: &GroupRowCtx, view_cards: bool, view_simple: bool| {
            templates
                .env()
                .get_template("partials/group_row.html")
                .unwrap()
                .render(minijinja::context! {
                    lang => "en", r => r, view_cards => view_cards, view_simple => view_simple,
                })
                .unwrap()
        };
        // The green tint, however minijinja escapes the slash of a class
        // that arrives through a variable.
        let green =
            |h: &str| h.matches("bg-success/10").count() + h.matches("bg-success&#x2f;10").count();
        for r in [&cleared, &bounded] {
            let html = render_in(r, false, false);
            assert!(html.contains("w-1 bg-success"), "{html}");
            for simple in [false, true] {
                let html = render_in(r, true, simple);
                assert!(green(&html) > 0, "simple={simple}: {html}");
            }
        }

        // The group header's accent is green only over a bounded field.
        let header = |r: &GroupRowCtx| {
            let s = GroupSectionCtx {
                id: 1,
                name: "Alpha".into(),
                description: None,
                location: None,
                rows: vec![r.clone()],
                field_count: 1,
                alarm_state: "Normal".into(),
                out_of_range: 0,
                fault_count: 0,
                member_events: Vec::new(),
                offline_hubs: Vec::new(),
            };
            templates
                .env()
                .get_template("partials/group_header.html")
                .unwrap()
                .render(minijinja::context! {
                    lang => "en", s => s, can_edit => false, is_first => true, is_last => true,
                })
                .unwrap()
        };
        const ACCENT: &str = r#"rounded-full shrink-0 bg-success""#;
        assert!(!header(&cleared).contains(ACCENT), "{}", header(&cleared));
        assert!(header(&bounded).contains(ACCENT), "{}", header(&bounded));
    }

    /// §9.2: the fault axis has three inputs and a wired probe is none of
    /// them. The header counts faulty *rows*, once each.
    #[test]
    fn faults_are_offline_and_low_battery_and_never_a_wired_probe() {
        let mac = "AA:BB:CC:DD:EE:FF";

        let mut flat = beacon_with("NORMAL", &[("temperature", "NORMAL")]);
        flat.low_battery = Some(true);
        let row = beacon_row(
            1,
            "hub",
            "Hub",
            &flat,
            &[],
            None,
            &liveness([(DeviceScope::Beacon, mac, true)]),
        );
        assert_eq!(row.faults, ["offline", "low_battery"]);

        let mut healthy = beacon_with("NORMAL", &[("temperature", "NORMAL")]);
        healthy.low_battery = Some(false);
        assert!(
            beacon_row(1, "hub", "Hub", &healthy, &[], None, &Default::default())
                .faults
                .is_empty()
        );

        // A wired probe that is not reporting carries no fault of its own:
        // its only liveness signal is the Hub-scoped `DeviceOffline`, and
        // rendering that as if the probe were silent would be a lie about
        // which thing stopped.
        let probe = probe_row(
            1,
            "hub",
            "Hub",
            &SensorState {
                line: 0,
                connected: false,
                ..Default::default()
            },
            true,
        );
        assert!(!probe.connected);
        assert!(probe.faults.is_empty());
    }

    /// One faulty row counts once per group, however many faults it carries
    /// — the header answers "how many probes are in trouble".
    #[test]
    fn a_section_counts_faulty_rows_not_faults() {
        let state = state_with_one_node(&[("temperature", 4.0), ("humidity", 50.0)]);
        let sections = sections(
            &state,
            &[group(1, "Alpha")],
            &[node_member(1, "n1", None)],
            &Default::default(),
            &liveness([(DeviceScope::Node, "n1", true)]),
        );
        assert_eq!(sections[0].rows[0].faults, ["offline"]);
        assert_eq!(
            sections[0].fault_count, 1,
            "one row in trouble, not one per card"
        );
    }

    /// F1: the toolbar's fulltext matches the group name too, so searching
    /// for a group shows its rows instead of nothing.
    /// B1: every list matches a device by its description; one lookup
    /// serves the row tokens and ids of Monitoring, Limits and Trends.
    #[test]
    fn descriptions_resolve_every_row_token_shape() {
        let d = Descriptions(
            [
                ("node:a1b2".to_string(), "Freezer".to_string()),
                ("beacon:AA:BB:CC:DD:EE:01".to_string(), "Truck".to_string()),
                ("hub:proximos-1".to_string(), "Rack".to_string()),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(d.for_token("node:A1B2"), Some("Freezer"));
        assert_eq!(d.for_token("node:a1b2:Temperature"), Some("Freezer"));
        assert_eq!(d.for_token("beacon:aa:bb:cc:dd:ee:01"), Some("Truck"));
        assert_eq!(d.for_token("eye:AA:BB:CC:DD:EE:01:Humidity"), Some("Truck"));
        assert_eq!(d.for_token("hub:Proximos-1"), Some("Rack"));
        assert_eq!(d.for_token("line:3"), None);
        let ids = d.by_id(["node:a1b2:Humidity", "probe:x"]);
        assert_eq!(ids.len(), 1);
    }

    #[test]
    fn a_rows_search_haystack_carries_its_group_name() {
        let state = state_with_one_node(&[("temperature", 4.0)]);
        let sections = sections(
            &state,
            &[group(1, "Chladicí Boxy")],
            &[node_member(1, "n1", None)],
            &Default::default(),
            &Default::default(),
        );
        assert!(
            sections[0].rows[0].search.contains("chladicí boxy"),
            "lower-cased like every other part: {}",
            sections[0].rows[0].search
        );
    }

    /// The token addresses the row's fragment route, so it has to survive
    /// the round trip out to a URL and back — including a MAC, which is full
    /// of the very separator the encoding uses.
    #[test]
    fn a_member_token_round_trips_through_parse_token() {
        let cases = [
            probe_member(1, 3),
            node_member(1, "aabbccdd00000001", None),
            beacon_member(1, "AA:BB:CC:DD:EE:01"),
        ];
        for m in &cases {
            let token = member_token(m).expect("every member encodes");
            let parsed = parse_token(&token).expect("and decodes: {token}");
            assert!(
                parsed.matches(m),
                "{token} decoded to something else: {parsed:?}"
            );
        }
        assert_eq!(
            parse_token("beacon:AA:BB:CC:DD:EE:01"),
            Some(MemberRef::Beacon("AA:BB:CC:DD:EE:01".into())),
            "split on the FIRST colon only"
        );
        for junk in [
            "",
            "node",
            "node:",
            "probe:not-a-number",
            "nonsense:1",
            ":1",
        ] {
            assert_eq!(parse_token(junk), None, "{junk:?} is not a token");
        }
    }

    /// [`names_for_device`] resolved against a whole snapshot, the way
    /// `sse::handle_change` resolves it against the store — so these tests
    /// drive the same function an open connection does.
    fn names(change: &ChangeEvent, state: &PortalState) -> Vec<String> {
        let device = match change {
            ChangeEvent::NodeBatch { hostname }
            | ChangeEvent::BeaconBatch { hostname }
            | ChangeEvent::DeviceStatus { hostname, .. } => state.device(hostname),
            _ => None,
        };
        names_for_device(change, device)
    }

    /// Both ends of the nudge contract come from the same three functions:
    /// the row stamps `event`, and `handle_change` derives the name it sends.
    /// A pair that only agreed by coincidence would fail silently — the
    /// browser would simply never swap.
    #[test]
    fn a_change_event_names_the_rows_it_can_move() {
        let state = state_with_one_node(&[("temperature", 4.0)]);
        let row = &sections(
            &state,
            &[group(1, "Alpha")],
            &[node_member(1, "n1", None)],
            &Default::default(),
            &Default::default(),
        )[0]
        .rows[0];

        assert_eq!(
            names(
                &ChangeEvent::Node {
                    hostname: "hub".into(),
                    node_key: "n1".into()
                },
                &state
            ),
            vec![row.event.clone()],
            "the event the row is listening on"
        );
        assert_eq!(
            names(
                &ChangeEvent::NodeBatch {
                    hostname: "hub".into()
                },
                &state
            ),
            ["mon-node-hub-n1"],
            "a batch fans out to one name per Node in the snapshot"
        );
        assert_eq!(
            names(
                &ChangeEvent::SensorLine {
                    hostname: "hub".into(),
                    line: 3
                },
                &state
            ),
            ["mon-probe-hub-3"]
        );
        assert_eq!(
            names(
                &ChangeEvent::DeviceStatus {
                    hostname: "hub".into(),
                    online: false
                },
                &state
            ),
            (0..8)
                .map(|l| format!("mon-probe-hub-{l}"))
                .chain(["mon-node-hub-n1".to_string()])
                .collect::<Vec<_>>(),
            "a Hub going silent repaints its wired probes (UX-053) and its Nodes and Beacons (#30c)"
        );
        assert_eq!(
            names(
                &ChangeEvent::Beacon {
                    hostname: "hub".into(),
                    mac: "AA:BB:CC:DD:EE:01".into()
                },
                &state
            ),
            ["mon-beacon-hub-AA-BB-CC-DD-EE-01"],
            "colons are dashed for the DOM, as the beacon card's id is"
        );
        // Nothing a monitoring row renders, so nothing to nudge.
        for quiet in [
            ChangeEvent::Groups,
            ChangeEvent::MqttConnection { connected: true },
            ChangeEvent::NodeConfig {
                hostname: "hub".into(),
                node_key: "n1".into(),
            },
        ] {
            assert!(names(&quiet, &state).is_empty(), "{quiet:?} moves no row");
        }
    }

    /// Review round 2: `sse.rs` derives a batch's names from **one** device
    /// rather than from a fleet snapshot, because it runs once per open
    /// connection per reading. The property that makes that a pure
    /// optimisation is here — a batch never names anything outside the Hub it
    /// carries, so the second Hub the snapshot held was only ever cloned to
    /// be ignored.
    #[test]
    fn a_batch_names_only_its_own_hubs_entities() {
        let mut state = state_with_one_node(&[("temperature", 4.0)]);
        let mut other = DeviceState::new("hub2".into());
        other.online = true;
        other
            .nodes
            .insert("n9".into(), NodeState::new("n9".into(), 0));
        let mut tag = BeaconState::new("AA:BB:CC:DD:EE:09".into(), 0);
        tag.discovered = None;
        other.ble_sensors.insert(tag.mac.clone(), tag);
        state.devices.insert("hub2".into(), other);

        for batch in [
            ChangeEvent::NodeBatch {
                hostname: "hub".into(),
            },
            ChangeEvent::BeaconBatch {
                hostname: "hub".into(),
            },
        ] {
            let one_device = names_for_device(&batch, state.devices.get("hub"));
            assert_eq!(
                one_device,
                names(&batch, &state),
                "one device answers exactly as the whole fleet did: {batch:?}"
            );
            assert!(
                !one_device.iter().any(|n| n.contains("hub2")),
                "hub2's entities are not in {batch:?}'s names: {one_device:?}"
            );
            assert!(
                names_for_device(&batch, None).is_empty(),
                "a Hub the store no longer holds names nothing: {batch:?}"
            );
        }
        // And the Node batch does name the Hub's own Node, so the assertion
        // above is not passing on an empty list.
        assert_eq!(
            names_for_device(
                &ChangeEvent::NodeBatch {
                    hostname: "hub".into()
                },
                state.devices.get("hub")
            ),
            ["mon-node-hub-n1"]
        );
    }

    /// F9: the fault tooltip builds its key from the slug
    /// (`("monitoring.groups.fault_" ~ f)|t`), which
    /// `every_static_template_key_exists` cannot see. Before MB2 the first
    /// faulty row showed the operator the raw key.
    #[test]
    fn every_fault_tooltip_key_exists_in_both_catalogues() {
        // Every fault slug the row builders above can push.
        for slug in [FAULT_OFFLINE, FAULT_LOW_BATTERY, FAULT_CONFIG_SYNC] {
            let key = format!("monitoring.groups.fault_{slug}");
            for lang in crate::i18n::LANGS {
                assert_ne!(
                    crate::i18n::lookup(lang, &key),
                    key,
                    "{key} is missing from the {lang} catalogue"
                );
            }
        }
    }

    #[test]
    fn a_single_reading_is_not_a_trend() {
        assert!(SparkCtx::from_values(&[]).is_none());
        assert!(SparkCtx::from_values(&[21.0]).is_none());
    }

    #[test]
    fn values_map_onto_the_viewbox_low_at_the_bottom() {
        let s = SparkCtx::from_values(&[0.0, 10.0, 5.0]).expect("three readings");
        assert_eq!(s.min, 0.0);
        assert_eq!(s.max, 10.0);
        // x spreads across the full 100 width; y is inverted, so the minimum
        // sits low on the card and the maximum high.
        assert_eq!(s.points, "0.0,30.0 50.0,2.0 100.0,16.0");
    }

    /// A flat series has no spread to normalise against; it still draws, mid
    /// height, rather than dividing by zero or vanishing.
    #[test]
    fn a_flat_series_draws_a_flat_line() {
        let s = SparkCtx::from_values(&[7.0, 7.0, 7.0]).expect("flat");
        assert_eq!(s.points, "0.0,16.0 50.0,16.0 100.0,16.0");
    }

    #[test]
    fn a_long_series_is_downsampled_but_keeps_its_range() {
        let values: Vec<f64> = (0..500).map(f64::from).collect();
        let s = SparkCtx::from_values(&values).expect("many");
        assert_eq!(s.points.split(' ').count(), SPARK_POINTS);
        assert_eq!(s.max, 499.0, "max over the whole window");
        assert_eq!(s.min, 0.0, "min over the whole window, not the tail");
    }
}
