use super::*;

// ---------------------------------------------------------------------
// M5-G: Node (LoRaWAN STICKER) / Beacon (BLE EYE) context builders.
//
// Casing rule (per templates/partials/{node_card,beacon_card}.html and
// pages/node_detail.html's header comments): the CARD-LEVEL/detail-level
// overall `alarm_state` is normalized to the mixed-case set the templates
// switch on ("Normal"/"Warning"/.../"NeverConnected"), same mapping as
// `normalize_alarm_state` above for wired sensors. PER-FIELD alarm states
// (`f.alarm_state` on both card and detail field entries) are templates'
// own firmware-cased strings ("NORMAL"/"WARNING"/"CRITICAL"), compared
// verbatim in the Jinja (`{% if f.alarm_state == "CRITICAL" %}`) — these
// are read straight off `NodeState::field_alarm_states`/
// `BeaconState::field_alarm_states` with NO normalization.
// ---------------------------------------------------------------------

/// Node/Beacon overall `alarm_state` firmware string -> template mixed-case.
/// Unlike wired sensors' [`normalize_alarm_state`], there is no "never
/// actually reported" store-side fallback to reproduce here:
/// `NodeState`/`BeaconState::alarm_state` already default to the literal
/// `"NeverConnected"` (mixed-case) until a payload supplies one, so
/// upper-casing before matching handles both that default and any
/// upper-cased firmware value in one pass; anything unrecognized passes
/// through in its original casing (same "don't guess" stance as the sensor
/// mapping).
pub fn normalize_radio_alarm_state(raw: &str) -> String {
    match raw.to_ascii_uppercase().as_str() {
        "NORMAL" => "Normal".to_string(),
        "WARNING" => "Warning".to_string(),
        "CRITICAL" => "Critical".to_string(),
        "DISCONNECTED" => "Disconnected".to_string(),
        "RECONNECTING" => "Reconnecting".to_string(),
        "NEVERCONNECTED" => "NeverConnected".to_string(),
        _ => raw.to_string(),
    }
}

/// A Node/Beacon has no explicit `connected` model field (unlike
/// `SensorState`) — connectivity is folded into the overall `alarm_state`
/// firmware enum instead (ws.md). Derived here for the templates' online
/// dot: "connected" is any state other than Disconnected/Reconnecting/
/// NeverConnected. Takes the *normalized* state so callers don't
/// re-implement the uppercasing.
///
/// `pub` because `monitoring.rs` open-coded the same `!matches!` four times
/// before MB0 split the value class off the connectivity one, and two copies
/// of this rule drifting apart is exactly the bug that split exists to stop.
pub fn radio_connected(normalized_alarm_state: &str) -> bool {
    !matches!(
        normalized_alarm_state,
        "Disconnected" | "Reconnecting" | "NeverConnected"
    )
}

/// Shared shape for a stored/state threshold, matching
/// `partials/limits_editor.html`'s `t` context and the chart JSON's
/// `thresholds` object (Node side; the 4 range bounds, no alarm_low/high —
/// see that partial's header for why radio thresholds differ from the wired
/// `limits_editor.html`).
#[derive(Debug, Clone, Serialize)]
pub struct ThresholdEntryCtx {
    pub critical_low: Option<f64>,
    pub warning_low: Option<f64>,
    pub warning_high: Option<f64>,
    pub critical_high: Option<f64>,
    pub enabled: bool,
    /// The field's own hysteresis, `None` = the default of its quantity
    /// (plan/control/alarm_hysteresis.md §5). The form pre-fills it.
    pub hysteresis: Option<f64>,
}

impl ThresholdEntryCtx {
    /// Whether a value has something to be "normal" against: a switched-on
    /// limit set with at least one bound in it. A field enabled with all four
    /// bounds cleared, or bounded but switched off, has none (UX-033). The one
    /// rule for the Monitoring cards (UX-055) and the Node/Beacon detail
    /// header (D3).
    pub fn bounded(&self) -> bool {
        self.enabled
            && [
                self.critical_low,
                self.warning_low,
                self.warning_high,
                self.critical_high,
            ]
            .iter()
            .any(Option::is_some)
    }

    /// The same band in the unit `f` is displayed in (UX-038: pressure in
    /// hPa). Identity for every field shown as the device sends it.
    pub fn displayed(self, f: &fiber_protocol::LorawanFieldDef) -> Self {
        let d = |v: Option<f64>| v.map(|v| f.to_display(v));
        ThresholdEntryCtx {
            critical_low: d(self.critical_low),
            warning_low: d(self.warning_low),
            warning_high: d(self.warning_high),
            critical_high: d(self.critical_high),
            enabled: self.enabled,
            // A width, so scaled without the offset.
            hysteresis: self.hysteresis.map(|h| f.to_display(h) - f.to_display(0.0)),
        }
    }
}

/// [`merge_node_thresholds`] with every band in its field's displayed unit —
/// what a page, a form or a chart shows. The send walk and the draft banner
/// compare against wire values and call [`merge_node_thresholds`] instead.
pub fn displayed_node_thresholds(
    state_thresholds: Option<&[NodeFieldThreshold]>,
    db_thresholds: &[FieldThreshold],
) -> BTreeMap<String, ThresholdEntryCtx> {
    merge_node_thresholds(state_thresholds, db_thresholds)
        .into_iter()
        .map(|(field, t)| {
            let t = match fiber_protocol::lorawan_field(&field) {
                Some(f) => t.displayed(f),
                None => t,
            };
            (field, t)
        })
        .collect()
}

/// [`ThresholdEntryCtx::bounded`] over a device's fields: the detail
/// header's `has_limits`.
pub fn any_bounded<'a>(thresholds: impl IntoIterator<Item = &'a ThresholdEntryCtx>) -> bool {
    thresholds.into_iter().any(ThresholdEntryCtx::bounded)
}

impl From<&FieldThreshold> for ThresholdEntryCtx {
    fn from(t: &FieldThreshold) -> Self {
        ThresholdEntryCtx {
            critical_low: t.critical_low,
            warning_low: t.warning_low,
            warning_high: t.warning_high,
            critical_high: t.critical_high,
            enabled: t.enabled,
            hysteresis: t.hysteresis,
        }
    }
}

/// The Node threshold precedence rule, `portal_domain::merge_node_thresholds`,
/// applied to the two shapes this binary holds them in: the firmware echo on
/// `NodeState::field_thresholds` and the `field_thresholds` rows (scope
/// `node`). The rule itself — echoed bounds win, the stored `enabled` wins,
/// a field in neither source is absent — lives and is tested in the domain
/// crate; this only adapts the types.
pub fn merge_node_thresholds(
    state_thresholds: Option<&[NodeFieldThreshold]>,
    db_thresholds: &[FieldThreshold],
) -> BTreeMap<String, ThresholdEntryCtx> {
    let echo = state_thresholds.map(|list| {
        list.iter().map(|t| {
            (
                t.field.as_str(),
                ThresholdBounds {
                    critical_low: t.critical_low,
                    warning_low: t.warning_low,
                    warning_high: t.warning_high,
                    critical_high: t.critical_high,
                },
            )
        })
    });
    let stored = db_thresholds.iter().map(|t| {
        (
            t.field.as_str(),
            ThresholdBounds {
                critical_low: t.critical_low,
                warning_low: t.warning_low,
                warning_high: t.warning_high,
                critical_high: t.critical_high,
            },
            t.enabled,
        )
    });
    portal_domain::merge_node_thresholds(echo, stored)
        .into_iter()
        .map(|(field, t)| {
            let mut t = ThresholdEntryCtx::from(t);
            t.hysteresis = db_thresholds
                .iter()
                .find(|r| r.field == field)
                .and_then(|r| r.hysteresis);
            (field, t)
        })
        .collect()
}

impl From<EffectiveThreshold> for ThresholdEntryCtx {
    fn from(t: EffectiveThreshold) -> Self {
        ThresholdEntryCtx {
            critical_low: t.bounds.critical_low,
            warning_low: t.bounds.warning_low,
            warning_high: t.bounds.warning_high,
            critical_high: t.bounds.critical_high,
            enabled: t.enabled,
            hysteresis: None,
        }
    }
}

/// `partials/node_card.html`'s per-field entry — value is non-optional
/// (only fields the Node has actually reported make it onto the card) and
/// `alarm_state` is the raw firmware string, defaulting to `"NORMAL"` when
/// the field has no entry in `field_alarm_states` yet (a field that has
/// never been reported in-alarm has no reason to render as anything else).
#[derive(Debug, Clone, Serialize)]
pub struct NodeFieldCardCtx {
    pub name: String,
    pub label: String,
    pub value: f64,
    pub unit: String,
    pub alarm_state: String,
}

/// The derived link-quality rung (devices.node.md §2.5). One indicator from
/// RSSI *and* SNR, because either alone misreads a link: a strong-but-noisy
/// signal and a weak-but-clean one are different problems.
///
/// The ladder is not uniform, and that is from the specification rather than
/// an oversight — `Excellent` and `Good` require **both** floors, `Weak`
/// requires **either**. `Unknown` is not a rung: it means a measurement is
/// missing, which is a different statement from "the link is bad".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalLevel {
    Excellent,
    Good,
    Weak,
    Critical,
    Unknown,
}

impl SignalLevel {
    /// Rank for the "by signal strength" ordering, ascending = worst first,
    /// so the links needing attention lead when the column is sorted up.
    ///
    /// `Unknown` ranks *after* `Excellent` rather than before `Critical`: a
    /// Node nobody has measured is not evidence of a bad link, and hoisting
    /// it to the top would bury the genuinely critical ones underneath it.
    pub fn severity_rank(self) -> u8 {
        match self {
            SignalLevel::Critical => 0,
            SignalLevel::Weak => 1,
            SignalLevel::Good => 2,
            SignalLevel::Excellent => 3,
            SignalLevel::Unknown => 4,
        }
    }
}

/// Why a Node is on the **operational** axis.
///
/// devices.node.md §2: the operational class and the value class are two
/// independent axes and are never merged. A Node reporting a perfectly
/// in-range temperature over a dead link is not "normal", and a Node
/// reporting an out-of-range temperature over a healthy link is not
/// "faulty" — collapsing them loses exactly the distinction an operator
/// acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeFault {
    /// B2's per-Node detector says this Node has gone quiet.
    Offline,
    /// The Node's latest backfill job ran out of retries: three
    /// cadence-spaced downlinks went out and none was answered, so that
    /// window of history is not coming (devices.backend.md §6.3). Before §19
    /// this raised `SystemAlertKind::NodeHistoryUnavailable` into the
    /// notifier and nothing else, so on a deployment with no recipients
    /// configured it reached nobody at all (F12). Read off
    /// `NodeState::history_unavailable`, which `backfill.rs` syncs from the
    /// jobs table.
    HistoryUnavailable,
    /// A field the Node has reported an alarm state for carries no value —
    /// devices.node.md §2: "a null measurement is a fault, not
    /// out-of-range".
    MissingMeasurement,
}

/// One band the Node list's threshold accordion offers
/// (`partials/node_thresholds_row.html`).
///
/// The rule is the bulk panel's, and for the same reason: the registry
/// calls twelve fields thresholdable, but a plain STICKER reports three.
/// Listing all twelve would put nine empty sliders under every row. A field
/// is offered when the Node has REPORTED it or already carries a band for
/// it — and the second half matters, because a band on a field that has
/// gone quiet must stay editable.
#[derive(Debug, Clone, Serialize)]
pub struct NodeThresholdFieldCtx {
    pub name: String,
    pub label: String,
    pub unit: String,
    /// The latest reading, so the slider can draw its needle. `None` for a
    /// field that only has a stored band.
    pub value: Option<f64>,
    /// What is stored now — `partials/limits_editor.html`'s `t`.
    pub threshold: Option<ThresholdEntryCtx>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeCardCtx {
    /// The Hub that owns this Node, stamped by the list builder so one list
    /// can hold rows from several Hubs (devices.hub.md §12 E). `None` on
    /// every other path, where the template falls back to the page-level (or
    /// SSE-supplied) `hostname` — the row id and its `sse-swap` name are
    /// built from whichever is set.
    pub hostname: Option<String>,
    pub node_key: String,
    pub name: Option<String>,
    pub serial_number: Option<String>,
    pub location: Option<String>,
    pub last_seen: Option<String>,
    pub alarm_state: String,
    pub rssi: Option<i64>,
    pub snr: Option<f64>,
    pub battery: Option<f64>,
    /// The same volts as `battery`, pre-formatted `x.yy V` for the row
    /// (devices.node.md §2.6 asks for two decimals everywhere a Node battery
    /// is shown). minijinja has no number-format filter, and `round(2)`
    /// prints `3.6` where the spec wants `3.60`, so the string is built here
    /// — the shape `beacon_ctx` already uses for the same reason.
    pub battery_label: Option<String>,
    pub connected: bool,
    pub fields: Vec<NodeFieldCardCtx>,
    /// `"lorawan"` | `"p2p"` — drives the transport badge (M8,
    /// plan/viewer/p2p_nodes.md §9).
    pub transport: portal_domain::Transport,
    /// P2P session lifecycle from the retained `info` document (devices.node.md
    /// §6): `detach_pending` and `stale` are the two the row has to say
    /// something about — a removal that is not yet confirmed, and one that
    /// never was. `None` on LoRaWAN and before the first join.
    pub session_state: Option<portal_domain::SessionState>,

    // ---- B1 (devices.backend.md §4, gap A) --------------------------------
    /// The gateways that heard this Node. Drives both the column and its
    /// checklist filter. A P2P Node's one entry is the Hub's own modem, named
    /// by the Hub's hostname.
    pub gateways: Vec<NodeGatewayCtx>,
    /// The Node's home gateway, as the list's Gateway column shows it (devices.node.md §5,
    /// dodatek 2026-10-13): [`HomeGateways::of`]. `None` from [`node_card_ctx`], which is pure;
    /// [`NodeRowData::card`] fills it.
    pub home_gateway: Option<String>,
    /// `None` = operationally healthy. See [`NodeFault`] for why this is not
    /// folded into `alarm_state`.
    pub fault: Option<NodeFault>,
    /// From the Node's own DeviceInfo — the v2 `device_info` document, else
    /// the v1 retained `info` — which is where a Node's firmware version
    /// lives. `NodeState` carries none (the `version` on `DeviceState` is the
    /// *Hub*'s).
    pub fw_version: Option<String>,
    /// Operator-supplied, from `device_meta`. Included in the fulltext
    /// filter, which is why it is on the row and not only on the detail page.
    pub description: Option<String>,
    /// When the operator registered this Node — the list's default sort,
    /// newest first. From `device_meta`, *not* `NodeState::added_at`: that
    /// one is a store-clock first-seen that resets with the process.
    pub created_at: Option<String>,
    /// Per-field threshold state, so the `Not set` / High-Low x
    /// Warning-Critical filter evaluates client-side without a round trip
    /// per row. Keys MAY be absent — a field in neither the firmware echo
    /// nor the database simply has no entry.
    pub thresholds: BTreeMap<String, ThresholdEntryCtx>,
    /// The bands the row's accordion offers — see [`NodeThresholdFieldCtx`].
    /// A subset of `thresholds` keys plus whatever the Node reports, so it
    /// is derived from both and neither alone.
    pub threshold_fields: Vec<NodeThresholdFieldCtx>,
    pub signal: SignalLevel,

    // ---- B4 (devices.backend.md §7) ---------------------------------------
    /// How many keys this Node's unsent-configuration draft holds
    /// (devices.node.md §3.5). Zero = no draft. On the row because the
    /// whole point of a shared draft is that a colleague can see one exists
    /// without opening every Node.
    pub pending_count: i64,
    /// Registered, never heard from — devices.node.md §5's
    /// "registered — waiting for join". See [`waiting_for_join`] for why
    /// this is derived rather than stored, and why it is not a fault.
    pub waiting_for_join: bool,
    /// The calibration icon beside the name (calibration.cs.md §6, OQ 3,
    /// UI amendment 2026-10-14): the level (`valid` green, both warnings
    /// orange, `expired` red), `None` = no certificate (grey). Neither a fault nor a value state — the
    /// row's other two axes do not read it.
    pub calibration: Option<&'static str>,
}

/// `partials/node_card.html`'s `n` context: the FEW chartable continuous
/// fields this Node actually reports (registry `Kind::Continuous` +
/// `chartable`, i.e. every continuous field except `voltage`) that have a
/// value in `NodeState::fields`. `battery` is the registry `voltage` field
/// specifically (that field's own doc comment: "battery = voltage field").
pub fn node_card_ctx(input: NodeCardInputs<'_>) -> NodeCardCtx {
    let NodeCardInputs {
        node,
        info,
        device_info,
        meta,
        thresholds,
        offline,
        signal_bands,
        pending_count,
        has_readings,
        calibration,
    } = input;

    let alarm_state = normalize_radio_alarm_state(&node.alarm_state);
    let connected = node_connected(&alarm_state, offline);

    let fields: Vec<NodeFieldCardCtx> = fiber_protocol::lorawan_registry()
        .iter()
        .filter(|f| f.kind == fiber_protocol::LorawanFieldKind::Continuous && f.chartable)
        .filter_map(|f| {
            node.fields.get(f.name).map(|&value| NodeFieldCardCtx {
                name: f.name.to_string(),
                label: f.label.to_string(),
                value: f.to_display(value),
                unit: f.display_unit().unwrap_or("").to_string(),
                alarm_state: node
                    .field_alarm_states
                    .get(f.name)
                    .cloned()
                    .unwrap_or_else(|| "NORMAL".to_string()),
            })
        })
        .collect();
    // Registry order, unsorted: the list row shows the first two as its
    // Values lines (temperature, then humidity on a STICKER), and each line
    // keeps its place between uplinks (devices.node.md, amendment 2026-10-14 "unified device table").
    // The worst state still reaches the row through its stripe and colours.

    let merged = displayed_node_thresholds(node.field_thresholds.as_deref(), thresholds);
    let threshold_fields: Vec<NodeThresholdFieldCtx> = fiber_protocol::lorawan_registry()
        .iter()
        .filter(|f| f.thresholdable)
        // A switched-off sensor (#120) takes no bounds until it is on again.
        .filter(|f| !node.disabled_fields.contains_key(f.name))
        .filter_map(|f| {
            let value = node.fields.get(f.name).map(|&v| f.to_display(v));
            let threshold = merged.get(f.name).cloned();
            if value.is_none() && threshold.is_none() {
                return None;
            }
            Some(NodeThresholdFieldCtx {
                name: f.name.to_string(),
                label: f.label.to_string(),
                unit: f.display_unit().unwrap_or("").to_string(),
                value,
                threshold,
            })
        })
        .collect();

    NodeCardCtx {
        // Stamped by the caller when the list spans several Hubs; see the
        // field's own comment.
        hostname: None,
        node_key: node.node_key.clone(),
        name: node.name.clone(),
        serial_number: node.serial_number.clone(),
        location: node.location.clone(),
        last_seen: node.last_seen.clone(),
        alarm_state,
        rssi: node.rssi,
        snr: node.snr,
        battery: node.fields.get("voltage").copied(),
        battery_label: node
            .fields
            .get("voltage")
            .map(|volts| format!("{volts:.2} V")),
        connected,
        fields,
        transport: node.transport,
        session_state: node.session_state,
        gateways: node.gateways.iter().map(gateway_ctx).collect(),
        home_gateway: None,
        fault: node_fault(node, offline == Some(true)),
        fw_version: device_info
            .and_then(fw_version_from_info)
            .or_else(|| info.and_then(fw_version_from_info)),
        description: meta.and_then(|m| m.description.clone()),
        created_at: meta.map(|m| m.created_at.clone()),
        thresholds: merged,
        threshold_fields,
        signal: signal_level(node.rssi, node.snr, signal_bands),
        pending_count,
        waiting_for_join: waiting_for_join(node, info, has_readings),
        calibration: calibration_flag(calibration),
    }
}

/// The list icon's state (UI amendment 2026-10-14: drawn for every level,
/// `None` = no certificate, the grey icon).
pub(crate) fn calibration_flag(
    level: Option<crate::calibration::CalibrationLevel>,
) -> Option<&'static str> {
    level.map(|l| l.as_str())
}

/// One entry of the detail page's message history (devices.backend.md §4,
/// gap C) — a decoded view of the last-10 raw uplink ring.
///
/// `values` and `signal` are pulled out of the stored payload rather than
/// re-derived from live state on purpose: the ring is a record of *what
/// arrived*, and rendering it against current state would quietly rewrite
/// history every time the Node reported again.
#[derive(Debug, Clone, Serialize)]
pub struct NodeMessageCtx {
    /// `None` only for a timestamp outside the representable range, which
    /// means a corrupt row — the template renders it blank rather than
    /// inventing a date.
    pub received_at: Option<String>,
    /// `"reading"`, `"alarm"`, … as far as the payload names itself;
    /// `"message"` when it does not.
    pub kind: String,
    /// The LoRaWAN port the frame came on (2 telemetry, 3 alarm batch, 85 the command
    /// engine), when the payload says. The ring keeps ten per port.
    pub port: Option<i64>,
    /// Field name → value, in the order the payload carried them. Values
    /// come from the wire, so they go through [`super::json_for_template`].
    pub values: BTreeMap<String, minijinja::Value>,
    pub rssi: Option<i64>,
    pub snr: Option<f64>,
}

/// The "last alarm message" line: the most recent out-of-range or fault
/// event, with the value and the bound it crossed.
#[derive(Debug, Clone, Serialize)]
pub struct LastAlarmCtx {
    pub at: Option<String>,
    pub event_type: String,
    /// A v2 alarm's side and edge (`high`, `activate`) for a translated label (HS-13); the
    /// template falls back to `event_type` for anything it has no key for.
    pub side: Option<String>,
    pub edge: Option<String>,
    pub field: Option<String>,
    /// The field's unit from the registry, for the value and the bound.
    pub unit: Option<&'static str>,
    pub value: Option<f64>,
    /// The bound the value crossed, when the event carried one.
    pub threshold: Option<f64>,
}

/// Decodes one ring row for display. A payload that will not parse still
/// produces a row — the timestamp and the fact that *something* arrived are
/// the useful part, and dropping it would make the ring silently shorter
/// than the ten messages the operator was promised.
pub fn node_message_ctx(row: &portal_storage::NodeRawMessage) -> NodeMessageCtx {
    let payload: Option<serde_json::Value> = serde_json::from_str(&row.payload).ok();
    let obj = payload.as_ref().and_then(|p| p.as_object());

    let values = match obj
        .and_then(|o| o.get("fields"))
        .and_then(|f| f.as_object())
    {
        // An fPort-3 batch measures nothing: what it carries is its alarm events.
        Some(f) if f.is_empty() => obj
            .and_then(|o| o.get("events"))
            .and_then(|e| e.as_array())
            .map(|events| alarm_event_values(events))
            .unwrap_or_default(),
        Some(f) => f
            .iter()
            .map(|(k, v)| (k.clone(), super::json_for_template(v)))
            .collect::<BTreeMap<_, _>>(),
        // A command-engine frame has no `fields`: show what identifies it — its seq and paging
        // and the scalars it carries — never the raw body or the identity it repeats.
        None => obj
            .map(|o| {
                o.iter()
                    .filter(|(k, v)| {
                        !matches!(
                            k.as_str(),
                            "dev_eui" | "received_at" | "kind" | "fport" | "body_hex"
                        ) && !v.is_object()
                            && !v.is_array()
                    })
                    .map(|(k, v)| (k.clone(), super::json_for_template(v)))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default(),
    };

    NodeMessageCtx {
        received_at: unix_to_rfc3339(row.received_at),
        kind: obj
            .and_then(|o| o.get("type").or_else(|| o.get("kind")))
            .and_then(|v| v.as_str())
            .unwrap_or(if payload.is_some() {
                "message"
            } else {
                "unparsed"
            })
            .to_string(),
        port: obj.and_then(|o| o.get("fport")).and_then(|v| v.as_i64()),
        values,
        rssi: obj.and_then(|o| o.get("rssi")).and_then(|v| v.as_i64()),
        snr: obj.and_then(|o| o.get("snr")).and_then(|v| v.as_f64()),
    }
}

/// A ring row's values for an alarm batch: quantity → `edge side value`, e.g.
/// `temperature` → `activate high 27.79`. Two events on one quantity keep apart by slot.
fn alarm_event_values(events: &[serde_json::Value]) -> BTreeMap<String, minijinja::Value> {
    let mut out = BTreeMap::new();
    for obj in events.iter().filter_map(|e| e.as_object()) {
        let Some(event_type) = obj.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        let Some(side) = crate::persist::v2_alarm_side(event_type, obj) else {
            continue;
        };
        let text = |k: &str| {
            obj.get(k)
                .map(|v| v.to_string().trim_matches('"').to_string())
        };
        let slot = text("slot").unwrap_or_default();
        let described = [text("event"), side, text("value")]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let mut key = text("quantity").unwrap_or_else(|| format!("slot {slot}"));
        if out.contains_key(&key) {
            key = format!("{key} (slot {slot})");
        }
        out.insert(key, minijinja::Value::from(described));
    }
    out
}

/// Picks the most recent alarm-ish event out of a `node_events` window.
///
/// "Alarm-ish" is decided by the event type carrying `alarm` or `fault`,
/// because `node_events` is a free-text-typed table shared by every decoded
/// event a Node emits — an allowlist of exact strings would go stale the
/// first time the firmware adds one. Rows arrive newest-first, so the first
/// match wins.
pub fn last_alarm_ctx(events: &[portal_storage::NodeEvent]) -> Option<LastAlarmCtx> {
    let payload_of = |e: &portal_storage::NodeEvent| -> Option<serde_json::Value> {
        e.payload
            .as_deref()
            .and_then(|p| serde_json::from_str(p).ok())
    };
    let (event, payload) = events.iter().find_map(|e| {
        let payload = payload_of(e);
        let t = e.event_type.to_ascii_lowercase();
        // A v2 `alarm_batch` is the header of an fPort-3 page, not an alarm: the alarm is
        // the event after it, which on the older wire shape is typed by its side.
        let alarmish = (t.contains("alarm") || t.contains("fault")) && t != "alarm_batch";
        let v2_alarm = payload
            .as_ref()
            .and_then(|p| p.as_object())
            .and_then(|o| crate::persist::v2_alarm_side(&e.event_type, o))
            .is_some();
        (alarmish || v2_alarm).then_some((e, payload))
    })?;
    let field_of = |key: &str| payload.as_ref().and_then(|p| p.get(key).cloned());
    let str_of = |key: &str| field_of(key).and_then(|v| v.as_str().map(str::to_string));
    // v2: `alarm` · side · edge, so the card says what crossed which way.
    let side = payload
        .as_ref()
        .and_then(|p| p.as_object())
        .and_then(|o| crate::persist::v2_alarm_side(&event.event_type, o))
        .flatten();
    let edge = str_of("event");
    let event_type = match (&side, &edge) {
        (Some(side), Some(edge)) => format!("alarm · {side} · {edge}"),
        (Some(side), None) => format!("alarm · {side}"),
        _ => event.event_type.clone(),
    };

    // A native fPort-3 alarm names its field `quantity`; the v1 shape `field`.
    let field = field_of("field")
        .or_else(|| field_of("quantity"))
        .and_then(|v| v.as_str().map(str::to_string));
    // Value and limit in the displayed unit, as on the rest of the page (UX-038).
    let def = field.as_deref().and_then(fiber_protocol::lorawan_field);
    let shown = |v: f64| def.map_or(v, |f| f.to_display(v));
    Some(LastAlarmCtx {
        at: unix_to_rfc3339(event.ts),
        event_type,
        edge: side.as_ref().and(edge),
        side,
        unit: def.and_then(|f| f.display_unit()),
        field,
        value: field_of("value").and_then(|v| v.as_f64()).map(shown),
        threshold: field_of("threshold")
            .or_else(|| field_of("bound"))
            .and_then(|v| v.as_f64())
            .map(shown),
    })
}

/// Everything the Node list needs from outside the store, loaded once per
/// render pass.
///
/// This exists because of §4's performance gate. `node_card_ctx` is pure and
/// takes its extras per row; the naive way to feed it is a database call per
/// row, which at the 500-Node fleet the spec calls *typical* is 1000 queries
/// per page render and another two per SSE row swap. Everything here is
/// instead two bulk queries per pass, and both tables are operator-edited
/// (thresholds, metadata) rather than telemetry, so they are small and
/// change rarely.
///
/// Held by value: it is built, read, and dropped inside one render pass, so
/// there is no invalidation problem to get wrong.
#[derive(Debug, Default)]
pub struct NodeRowData {
    pub meta: std::collections::HashMap<String, portal_storage::DeviceMeta>,
    pub thresholds: std::collections::HashMap<String, Vec<FieldThreshold>>,
    /// B2's verdict table for the whole fleet — see [`node_connected`] for
    /// why this is a three-valued answer and not a set of offline keys.
    pub liveness: crate::notify::offline::LivenessVerdicts,
    pub signal_bands: crate::config::SignalQualityConfig,
    /// B4: draft size per Node, from one grouped
    /// `count_pending_by_node` — a Node with no draft is simply absent.
    pub pending_counts: std::collections::HashMap<String, i64>,
    /// The Node keys this Hub has ever stored a reading for, from one
    /// grouped existence query (§21 review, item 8). Feeds
    /// [`waiting_for_join`]'s restart clause; absent = no stored readings,
    /// which is what a genuinely fresh registration looks like.
    pub nodes_with_readings: std::collections::HashSet<String>,
    /// The calibration level of each Node holding a certificate, off
    /// `CalibrationService`'s in-memory cache — no query.
    pub calibration: std::collections::HashMap<String, crate::calibration::CalibrationLevel>,
    /// What names each Node's home gateway — see [`HomeGateways`].
    pub home_gateways: HomeGateways,
}

/// What the Nodes list's Gateway column is resolved from (devices.node.md §5, dodatek
/// 2026-10-13): the gateway a Node is homed on, named by its Hub where the portal knows it.
#[derive(Debug, Default)]
pub struct HomeGateways {
    /// The Server's hostname: a P2P home on its own Northbridge (`local`) is named so.
    pub server: Option<String>,
    /// The Server's P2P sessions, lowercase DevEUI → home (`None` = `local`, else a Client's
    /// hostname); [`crate::radio::RadioControl::p2p_homes`].
    pub p2p: std::collections::HashMap<String, Option<String>>,
    /// Gateway EUI (lowercase) → the Hub it belongs to.
    pub hubs_by_eui: std::collections::HashMap<String, String>,
}

impl HomeGateways {
    /// The home gateway of `node`, or `None` when the portal does not know one.
    ///
    /// - P2P: the session's home — the Server for `local`, else the Client. A Node with no
    ///   session has not joined, and has no home yet.
    /// - LoRaWAN: the gateway that heard its last uplink best (highest RSSI; a gateway without
    ///   one ranks last), named by its Hub when the EUI is known, else the EUI itself.
    pub fn of(&self, node: &NodeState) -> Option<String> {
        match node.transport {
            portal_domain::Transport::P2p => {
                match self.p2p.get(&node.node_key.to_ascii_lowercase())? {
                    Some(client) => Some(client.clone()),
                    None => Some(
                        self.server
                            .clone()
                            .unwrap_or_else(|| control_radio::p2p::gateways::LOCAL.to_string()),
                    ),
                }
            }
            portal_domain::Transport::Lorawan => {
                let best = node
                    .gateways
                    .iter()
                    .filter_map(|g| g.gateway_id.as_deref().map(|id| (id, g.rssi)))
                    .filter(|(id, _)| !id.trim().is_empty())
                    .fold(None::<(&str, Option<i64>)>, |best, (id, rssi)| match best {
                        Some((_, b)) if b >= rssi => best,
                        _ => Some((id, rssi)),
                    })?;
                let eui = best.0.trim().to_ascii_lowercase();
                Some(self.hubs_by_eui.get(&eui).cloned().unwrap_or(eui))
            }
        }
    }
}

impl NodeRowData {
    /// One Node's row: [`node_card_ctx`] on [`Self::inputs`], with the home gateway.
    pub fn card(&self, node: &NodeState, device: &DeviceState) -> NodeCardCtx {
        let mut ctx = node_card_ctx(self.inputs(node, device));
        ctx.home_gateway = self.home_gateways.of(node);
        ctx
    }

    /// Assembles one Node's inputs. The two per-Node maps (`node_infos`,
    /// `node_device_infos`) are read from the device snapshot the caller
    /// already holds, so it is borrowed rather than copied in here.
    pub fn inputs<'a>(
        &'a self,
        node: &'a NodeState,
        device: &'a DeviceState,
    ) -> NodeCardInputs<'a> {
        NodeCardInputs {
            node,
            info: device.node_infos.get(&node.node_key),
            device_info: device
                .node_device_infos
                .get(&node.node_key)
                .map(|e| &e.info),
            meta: self.meta.get(&node.node_key),
            thresholds: self
                .thresholds
                .get(&node.node_key)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            offline: self
                .liveness
                .verdict(crate::notify::offline::DeviceScope::Node, &node.node_key),
            signal_bands: &self.signal_bands,
            pending_count: self
                .pending_counts
                .get(&node.node_key)
                .copied()
                .unwrap_or(0),
            has_readings: self.nodes_with_readings.contains(&node.node_key),
            calibration: self.calibration.get(&node.node_key).copied(),
        }
    }
}

/// Everything one Node row needs that is not on `NodeState`.
///
/// Passed as a struct rather than six positional arguments because three of
/// the six are `Option`s of different types that would be trivially
/// swappable at a call site. `node_card_ctx` stays pure — resolving these is
/// the caller's job, and doing it in bulk once per render pass rather than
/// once per row is what keeps a 500-row list off a per-row database query
/// (§4's performance gate).
pub struct NodeCardInputs<'a> {
    pub node: &'a NodeState,
    /// This Node's retained `info` payload (`DeviceState::node_infos`): the
    /// Node's DeviceInfo on the v1 path, the Hub's enrollment inventory on the
    /// v2 one — which is where `pairing_status` comes from.
    pub info: Option<&'a serde_json::Value>,
    /// This Node's last fPort-85 `Info` from the v2 `device_info` document
    /// (`DeviceState::node_device_infos`, node_reported_state.md §4). Read
    /// before `info` for anything the Node itself reports.
    pub device_info: Option<&'a serde_json::Value>,
    pub meta: Option<&'a portal_storage::DeviceMeta>,
    /// The stored rows for this Node; merged with the firmware echo by
    /// [`merge_node_thresholds`].
    pub thresholds: &'a [FieldThreshold],
    /// B2's verdict for this Node: `Some(true)` offline, `Some(false)`
    /// online, `None` no verdict yet — see [`node_connected`].
    pub offline: Option<bool>,
    pub signal_bands: &'a crate::config::SignalQualityConfig,
    /// B4: how many keys this Node's draft holds; 0 = none.
    pub pending_count: i64,
    /// The portal holds at least one stored reading for this Node — proof
    /// it has joined that survives a restart, unlike `NodeState::last_seen`.
    /// See [`waiting_for_join`].
    pub has_readings: bool,
    /// This Node's calibration level; `None` = no certificate uploaded.
    pub calibration: Option<crate::calibration::CalibrationLevel>,
}

/// The portal's one answer to "is this Node connected" (devices.backend.md
/// §19, F2 + F3).
///
/// **The detector overrides the Hub's echo.** `NodeState::alarm_state` is a
/// firmware enum the Hub last echoed, and it disagrees with the portal in
/// both directions: a Node silent past K × `interval_report` keeps its last
/// `NORMAL` echo, so B2's fault triangle appeared beside a green dot (F2);
/// and a Node fed only by the per-Node v2 envelope is never written at all,
/// so it stays `NeverConnected` for its whole life however recently it
/// reported (F3). Neither is a second axis — devices.node.md §3.1's "two
/// independent indicators" are *value* versus *operational*, not two
/// disagreeing answers to the same question.
///
/// The echo survives only as the fallback for a Node the detector holds no
/// verdict on: before the first sweep, with detection switched off for the
/// Node, or for one never heard from — where `NeverConnected` is exactly
/// right and the detector, by design, says nothing (B4 renders that case as
/// "waiting for join").
///
/// The dev mock publishes the v1 aggregate, so none of this is visible in
/// the dev loop; the unit tests beside this function are what hold the rule.
pub fn node_connected(alarm_state: &str, offline: Option<bool>) -> bool {
    match offline {
        Some(offline) => !offline,
        None => radio_connected(alarm_state),
    }
}

fn gateway_ctx(g: &NodeGatewayLink) -> NodeGatewayCtx {
    NodeGatewayCtx {
        gateway_id: g.gateway_id.clone(),
        rssi: g.rssi,
        snr: g.snr,
    }
}

/// The operational axis, in precedence order: offline first, because a Node
/// nobody can hear is the fault that explains every other symptom — a
/// terminal history job and a self-contradicting reading are both things
/// silence would predict.
///
/// `HistoryUnavailable` outranks `MissingMeasurement` because it names a
/// concrete thing an operator can do something about (the window is gone;
/// re-request it or accept the hole), while a missing measurement is the
/// narrowest signal here and usually a firmware quirk.
///
/// The last case is narrow on purpose. devices.node.md §2 says "a null
/// measurement is a fault", and the only null this portal can actually
/// *identify* is a field the Node has reported an alarm state for while
/// carrying no value — that is a Node contradicting itself, which is
/// unambiguous. The broader reading, "a field this Node model should report
/// and does not", needs the per-model expected-field set that only the
/// generated FW parameter registry would supply (Node gap G, deliberately
/// not issued — devices.backend.md §1). Guessing it from the 25-field
/// registry would mark every Node faulty for every field it legitimately
/// does not have.
pub(super) fn node_fault(node: &NodeState, offline: bool) -> Option<NodeFault> {
    if offline {
        return Some(NodeFault::Offline);
    }
    if node.history_unavailable {
        return Some(NodeFault::HistoryUnavailable);
    }
    let contradicts_itself = node
        .field_alarm_states
        .keys()
        .any(|field| !node.fields.contains_key(field));
    contradicts_itself.then_some(NodeFault::MissingMeasurement)
}

/// A Node's own firmware version, dug out of the retained DeviceInfo blob.
///
/// The payload is passed through verbatim from the wire, so the key is
/// tried under both the flat and the nested spelling the mock and the
/// firmware have each used. A miss is `None`, never a guess.
fn fw_version_from_info(info: &serde_json::Value) -> Option<String> {
    info.get("fw_version")
        .or_else(|| info.get("info").and_then(|i| i.get("fw_version")))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// A Node's firmware version as `(major, minor, patch)`: its own `Info` (the v2 `device_info`
/// document: `fw_major`/`fw_minor`/`fw_patch`, else `fw_version`), else the retained `info`
/// payload's `fw_version`. `None` when neither states one readably.
pub fn node_fw_version(device: &DeviceState, node_key: &str) -> Option<(u64, u64, u64)> {
    device
        .node_device_infos
        .get(node_key)
        .and_then(|e| fw_version_of_info(&e.info))
        .or_else(|| {
            device
                .node_infos
                .get(node_key)
                .and_then(fw_version_from_info)
                .as_deref()
                .and_then(parse_fw_version)
        })
}

/// The firmware version one `Info` document states: `fw_major`/`fw_minor`/`fw_patch`, else
/// `fw_version`. `None` when it states none readably.
pub fn fw_version_of_info(info: &serde_json::Value) -> Option<(u64, u64, u64)> {
    let n = |key: &str| info.get(key).and_then(serde_json::Value::as_u64);
    n("fw_major")
        .map(|major| {
            (
                major,
                n("fw_minor").unwrap_or(0),
                n("fw_patch").unwrap_or(0),
            )
        })
        .or_else(|| {
            fw_version_from_info(info)
                .as_deref()
                .and_then(parse_fw_version)
        })
}

/// `v1.4.0`, `1.5.0-rc1`: the leading numbers of up to three dot-separated parts.
fn parse_fw_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().trim_start_matches(['v', 'V']);
    let mut parts = text.split('.').map(|p| {
        let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u64>().ok()
    });
    let major = parts.next()??;
    let minor = parts.next().flatten().unwrap_or(0);
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

/// Derives the one link-quality rung from RSSI and SNR against the
/// configured floors (devices.node.md §2.5).
///
/// Either measurement missing yields [`SignalLevel::Unknown`]: the top two
/// rungs need both, and reading an absent SNR as "fails the floor" would
/// report a healthy link as weak.
pub fn signal_level(
    rssi: Option<i64>,
    snr: Option<f64>,
    bands: &crate::config::SignalQualityConfig,
) -> SignalLevel {
    let (Some(rssi), Some(snr)) = (rssi, snr) else {
        return SignalLevel::Unknown;
    };
    if rssi >= bands.excellent.rssi && snr >= bands.excellent.snr {
        SignalLevel::Excellent
    } else if rssi >= bands.good.rssi && snr >= bands.good.snr {
        SignalLevel::Good
    } else if rssi >= bands.weak.rssi || snr >= bands.weak.snr {
        // `or`, not `and` — from the specification's own table.
        SignalLevel::Weak
    } else {
        SignalLevel::Critical
    }
}

/// `pages/node_detail.html`'s `n.gateways[]` entry.
#[derive(Debug, Clone, Serialize)]
pub struct NodeGatewayCtx {
    pub gateway_id: Option<String>,
    pub rssi: Option<i64>,
    pub snr: Option<f64>,
}

/// `pages/node_detail.html`'s `n.fields[]` entry — the FULL registry-driven
/// list (all 25 LoRaWAN fields), unlike the card's few-fields list.
/// `unit` is `""` (not `None`) for the registry's unitless fields (counters,
/// events) — the template's own truthiness check (`{% if f.unit %}`) treats
/// an empty string as falsy, matching the "" = unset convention used
/// elsewhere. `alarm_state` stays the raw firmware string (or `None` if the
/// field has never appeared in `field_alarm_states`) — deliberately NOT
/// normalized, per this work package's brief.
#[derive(Debug, Clone, Serialize)]
pub struct NodeFieldDetailCtx {
    pub name: String,
    pub label: String,
    pub unit: String,
    pub value: Option<f64>,
    pub alarm_state: Option<String>,
    pub thresholdable: bool,
    pub chartable: bool,
    /// A count (`motion_count`, ...): `value` comes from the Node's counters and shows as a
    /// whole number (GitLab #134).
    pub count: bool,
    /// `false` = the sensor is switched off (GitLab #120): `value` is the last one the Node
    /// sent, and only the sensor settings table shows the field.
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeDetailCtx {
    pub node_key: String,
    pub name: Option<String>,
    pub serial_number: Option<String>,
    pub location: Option<String>,
    pub last_seen: Option<String>,
    pub alarm_state: String,
    pub rssi: Option<i64>,
    pub snr: Option<f64>,
    pub gateways: Vec<NodeGatewayCtx>,
    pub counters: BTreeMap<String, i64>,
    /// The Node's uplink frame counter, whatever its carrier calls it ([`uplink_counter`]).
    pub uplink_counter: Option<i64>,
    /// The device's own DeviceInfo, verbatim — see [`device_info_for_detail`]
    /// for which document that is. Converted through
    /// [`super::json_for_template`], not serialized raw: see that function
    /// for what `arbitrary_precision` does to a number on the way in.
    pub info: Option<minijinja::Value>,
    /// RFC3339, when the Hub received that DeviceInfo — devices.node.md
    /// UI-9: it is cached state and the card says how old. `None` on the v1
    /// path, whose `info` carries no receive time.
    pub info_received_at: Option<String>,
    /// The card's decoded view of `info`'s raw numbers — see [`DeviceInfoView`].
    pub info_view: DeviceInfoView,
    /// The Node's active sensors from its config info (WP3); `None` before the Node has
    /// reported one.
    pub sensors: Option<ActiveSensorsCtx>,
    pub fields: Vec<NodeFieldDetailCtx>,
    /// `"lorawan"` | `"p2p"` — drives the transport badge (M8,
    /// plan/viewer/p2p_nodes.md §9).
    pub transport: portal_domain::Transport,
    /// Session identity (devices.node.md §3.1/§3.7), filled from the retained
    /// `info` document by the store; `None` where the Hub published none (the
    /// LoRaWAN arm today, a P2P Node before its first join). `last_join_at` is
    /// RFC 3339 for the template's `short_dt`.
    pub dev_addr: Option<String>,
    pub net_id: Option<String>,
    pub session_state: Option<portal_domain::SessionState>,
    pub last_join_at: Option<String>,

    // ---- §19 (F2/F4/F12) --------------------------------------------------
    /// The one liveness verdict, same rule as the list — [`node_connected`].
    pub connected: bool,
    /// `None` = operationally healthy. Drives devices.node.md §3.1b's fault
    /// banner; see [`NodeFault`] for why this is not folded into
    /// `alarm_state`.
    pub fault: Option<NodeFault>,
    /// Since when the fault has stood, for the banner's second line. Only an
    /// offline fault has an honest answer — the Node's own `last_seen` — so
    /// the other two variants carry `None` rather than a timestamp invented
    /// out of when the page happened to be rendered.
    pub fault_since: Option<String>,
    /// B4: registered and never heard from (devices.node.md §5). The same
    /// derivation the list row uses — see [`waiting_for_join`].
    pub waiting_for_join: bool,
    /// The Node's last `device_status` (P2P `DevStatusAns`: battery, downlink RSSI/SNR, TX
    /// power, uptime), verbatim; `None` before the first answer. Set by the caller with
    /// [`NodeDetailCtx::with_device_status`] (control-radio p2p_control_commands.md §5).
    pub device_status: Option<minijinja::Value>,
    /// The Node's last crash record (GitLab #135): from the DeviceInfo shown when it carries
    /// one, else the newest stored one ([`NodeDetailCtx::with_last_fault`]). `None` = the
    /// Node has never reported a crash the portal knows of.
    pub last_fault: Option<LastFaultCtx>,
}

impl NodeDetailCtx {
    pub fn with_device_status(mut self, doc: Option<&serde_json::Value>) -> Self {
        self.device_status = doc.map(super::json_for_template);
        self
    }

    /// Adds the newest stored crash record (a `fatal_error` row of `node_events`, see
    /// [`fault_record`]) when it is newer than the one the current DeviceInfo carries, which
    /// it is once a later Info without the fault has replaced that one.
    pub fn with_last_fault(mut self, event: Option<&portal_storage::NodeEvent>) -> Self {
        let stored = event.and_then(|e| {
            let doc: serde_json::Value = serde_json::from_str(e.payload.as_deref()?).ok()?;
            let received = doc
                .get("received_at")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .or_else(|| super::unix_to_rfc3339(e.ts));
            LastFaultCtx::of(&doc, received.as_deref()).map(|f| (e.ts, f))
        });
        if let Some((ts, stored)) = stored {
            let current_ts = self
                .last_fault
                .as_ref()
                .and_then(|f| f.received_at.as_deref())
                .and_then(rfc3339_unix);
            if current_ts.is_none_or(|c| ts > c) {
                self.last_fault = Some(stored);
            }
        }
        self
    }
}

/// `node_events.event_type` of a stored crash record (GitLab #135). Deliberately neither
/// "alarm" nor "fault": [`last_alarm_ctx`] takes any event type containing either for an alarm.
pub const FATAL_ERROR_EVENT: &str = "fatal_error";

/// The Node's crash record (sticker-firmware v2.0.0 `Info` fields 20..22, node-firmware-v2.0.0.md
/// §7), as control-radio decodes it: `fault_reason` (Zephyr `K_ERR_*`), `fault_reason_name`,
/// `fault_pc` and `fault_lr` (`0x%08x`). Sent only on the first boot after a fatal error.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LastFaultCtx {
    pub reason: Option<u64>,
    pub reason_name: Option<String>,
    pub pc: Option<String>,
    pub lr: Option<String>,
    /// RFC 3339, when the Hub received the Info that carried it.
    pub received_at: Option<String>,
}

impl LastFaultCtx {
    /// The record in a DeviceInfo document, or `None` when it carries none.
    pub fn of(info: &serde_json::Value, received_at: Option<&str>) -> Option<Self> {
        let text = |k: &str| match info.get(k)? {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => n.as_u64().map(|v| format!("0x{v:08x}")),
            _ => None,
        };
        let f = Self {
            reason: info.get("fault_reason").and_then(serde_json::Value::as_u64),
            reason_name: info
                .get("fault_reason_name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            pc: text("fault_pc"),
            lr: text("fault_lr"),
            received_at: received_at.map(str::to_string),
        };
        (f.reason.is_some() || f.reason_name.is_some() || f.pc.is_some() || f.lr.is_some())
            .then_some(f)
    }
}

/// The `fault_*` keys of a DeviceInfo, plus `reset_cause` and the receive time, as the
/// `fatal_error` event payload the portal stores (GitLab #135). `None` without a crash record.
pub fn fault_record(
    info: &serde_json::Map<String, serde_json::Value>,
    received_at: &str,
) -> Option<serde_json::Value> {
    let mut out = serde_json::Map::new();
    for key in ["fault_reason", "fault_reason_name", "fault_pc", "fault_lr"] {
        if let Some(v) = info.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    if out.is_empty() {
        return None;
    }
    if let Some(v) = info.get("reset_cause") {
        out.insert("reset_cause".to_string(), v.clone());
    }
    out.insert("received_at".to_string(), received_at.into());
    Some(serde_json::Value::Object(out))
}

fn rfc3339_unix(at: &str) -> Option<i64> {
    time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(time::OffsetDateTime::unix_timestamp)
}

/// `pages/node_detail.html`'s `n` context. `info`/`counters` are passed
/// through as cached by the store (`node_infos`/`NodeState::counters`) —
/// the page's header comment notes `info` is read defensively by the
/// template since firmware owns its shape.
///
/// `offline` is the same outside input the list row takes (see
/// [`NodeCardInputs`]); the history fault rides on the `NodeState` both read,
/// so the detail page and the row cannot disagree about a Node's operational
/// state.
pub fn node_detail_ctx(
    node: &NodeState,
    info: Option<&serde_json::Value>,
    device_info: Option<&NodeDeviceInfoEntry>,
    config: Option<&NodeConfigEntry>,
    offline: Option<bool>,
    stored_fields: &std::collections::HashSet<String>,
) -> NodeDetailCtx {
    let alarm_state = normalize_radio_alarm_state(&node.alarm_state);
    let connected = node_connected(&alarm_state, offline);
    let fault = node_fault(node, offline == Some(true));

    // Only what the Node measures, in its order (devices.node.md §3.4, dodatek 2026-09-24).
    // Without config info that is the board and what has arrived, and a stored reading counts
    // as arrived, so the page survives a portal restart that emptied `node.fields` (§3.4).
    let mut active = active_node_fields(config, node);
    if config
        .and_then(|e| declared_fields(e, Some(node)))
        .is_none()
    {
        for f in fiber_protocol::lorawan_registry() {
            if stored_fields.contains(f.name)
                && is_sensor_field(f)
                && !active.iter().any(|a| a.name == f.name)
            {
                active.push(ActiveField {
                    name: f.name.to_string(),
                    label: None,
                });
            }
        }
    }
    let fields = active
        .into_iter()
        .filter_map(|active| {
            let f = fiber_protocol::lorawan_field(&active.name)?;
            let parked = node.disabled_fields.get(f.name);
            let value = match parked {
                Some(last) => *last,
                // A count is in `counters`, not `fields` (GitLab #134).
                None => node.reading(f.name),
            };
            Some(NodeFieldDetailCtx {
                name: f.name.to_string(),
                label: active.label.unwrap_or_else(|| f.label.to_string()),
                unit: f.display_unit().unwrap_or("").to_string(),
                value: value.map(|v| f.to_display(v)),
                alarm_state: node.field_alarm_states.get(f.name).cloned(),
                thresholdable: f.thresholdable,
                chartable: f.chartable,
                count: f.kind == fiber_protocol::LorawanFieldKind::Counter,
                enabled: parked.is_none(),
            })
        })
        .collect();

    NodeDetailCtx {
        node_key: node.node_key.clone(),
        name: node.name.clone(),
        serial_number: node.serial_number.clone(),
        location: node.location.clone(),
        last_seen: node.last_seen.clone(),
        alarm_state,
        rssi: node.rssi,
        snr: node.snr,
        gateways: node
            .gateways
            .iter()
            .map(|g| NodeGatewayCtx {
                gateway_id: g.gateway_id.clone(),
                rssi: g.rssi,
                snr: g.snr,
            })
            .collect(),
        counters: node.counters.clone(),
        uplink_counter: uplink_counter(&node.counters),
        info: device_info_for_detail(info, device_info).map(super::json_for_template),
        info_received_at: device_info.map(|e| e.received_at.clone()),
        info_view: device_info_for_detail(info, device_info)
            .map(|doc| DeviceInfoView::of(doc, device_info.map(|e| e.received_at.as_str())))
            .unwrap_or_default(),
        sensors: active_sensors_ctx(config),
        fields,
        transport: node.transport,
        dev_addr: node.dev_addr.clone(),
        net_id: node.net_id.clone(),
        session_state: node.session_state,
        last_join_at: node.last_join_at.and_then(|at| {
            time::OffsetDateTime::from_unix_timestamp(at)
                .ok()?
                .format(&time::format_description::well_known::Rfc3339)
                .ok()
        }),
        connected,
        fault,
        fault_since: matches!(fault, Some(NodeFault::Offline))
            .then(|| node.last_seen.clone())
            .flatten(),
        waiting_for_join: waiting_for_join(node, info, !stored_fields.is_empty()),
        device_status: None,
        // The v2 DeviceInfo only: the v1 document has no crash record.
        last_fault: device_info
            .and_then(|e| LastFaultCtx::of(&e.info, Some(e.received_at.as_str()))),
    }
}

/// The Node's uplink frame counter from its counters, on either carrier: LoRaWAN reports it as
/// `fCnt` (N27; `fcnt` from an older Hub), P2P as `counter` (control-radio `Reception`). One key
/// for the template, so a P2P Node's counter no longer reads "–" (PU-3).
pub fn uplink_counter(counters: &BTreeMap<String, i64>) -> Option<i64> {
    ["fCnt", "fcnt", "counter"]
        .iter()
        .find_map(|key| counters.get(*key).copied())
}

/// Which document the detail page's DeviceInfo card shows.
///
/// The v2 `device_info` document when there is one. Otherwise the retained
/// `info` — but only when it *is* a DeviceInfo, which it is on the v1 path
/// (`StickerInfo`) and is not on the v2 one, where `info` is the Hub's
/// enrollment inventory (node_reported_state.md §0 finding 2). That
/// inventory always carries `pairing_status` (null until the first frame)
/// and a v1 DeviceInfo never does, so the key tells them apart; showing the
/// inventory would render an empty card instead of "no device info yet".
fn device_info_for_detail<'a>(
    info: Option<&'a serde_json::Value>,
    device_info: Option<&'a NodeDeviceInfoEntry>,
) -> Option<&'a serde_json::Value> {
    device_info
        .map(|e| &e.info)
        .or_else(|| info.filter(|i| i.get("pairing_status").is_none()))
}

// ---- The Node's sensors (devices.node.md §3.4, dodatek 2026-09-24) -----------------------
//
// Which quantities a Node measures comes from its config info (`sensors.cap_*` and the 1-Wire
// slot types), the way the firmware composes its telemetry (sticker-firmware v2.0.0
// `app_compose.c`). One table each, so the sensor-type registry the firmware plans (#431,
// `sensor_types.yml`) can later generate them.

/// The onboard board's sensors: always present, always on the wire.
///
/// Not the battery (plan/portal/threshold_defaults.md §5): `voltage` is on the wire too, but
/// it is the Node's battery, shown as such (the list's battery column, the device info card),
/// and the Node alarms on it itself. No sensor list names it; see [`is_sensor_field`].
pub(crate) const BOARD_FIELDS: &[&str] = &["temperature", "humidity"];

/// Whether a registry field is one of the Node's sensors at all: everything but the battery
/// group (threshold_defaults.md §5).
fn is_sensor_field(f: &fiber_protocol::LorawanFieldDef) -> bool {
    f.group != fiber_protocol::LorawanFieldGroup::Battery
}

/// A `sensors.cap_*` sensor and the telemetry fields it adds.
///
/// *Amended 2026-10-14 (v2.0.0, #499):* the GP pins are three enum caps
/// (`cap_gpio_mode`, `cap_gpio_input_a/_b`) instead of the old booleans, so a sensor names
/// the `values` of its cap that turn it on (empty: a boolean cap) and its own `label`, the
/// `monitoring.sensor.<label>` the card shows. Firmware `app_compose.c`: a digital or pulse
/// input counts, an analog one reports its voltage.
pub(crate) struct CapSensor {
    pub cap: &'static str,
    pub values: &'static [&'static str],
    pub label: &'static str,
    pub fields: &'static [&'static str],
}

const fn bool_cap(cap: &'static str, fields: &'static [&'static str]) -> CapSensor {
    CapSensor {
        cap,
        values: &[],
        label: cap,
        fields,
    }
}

pub(crate) const CAP_SENSORS: &[CapSensor] = &[
    bool_cap("cap_barometer", &["pressure", "altitude"]),
    bool_cap("cap_light_sensor", &["illuminance"]),
    bool_cap("cap_accelerometer", &["orientation", "accel_motion_count"]),
    CapSensor {
        cap: "cap_gpio_mode",
        values: &["pir"],
        label: "cap_pir_detector",
        fields: &["motion_count"],
    },
    bool_cap("cap_hall_left", &["hall_left_count"]),
    bool_cap("cap_hall_right", &["hall_right_count"]),
    CapSensor {
        cap: "cap_gpio_input_a",
        values: &["digital", "pulse"],
        label: "cap_input_a",
        fields: &["input_a_count"],
    },
    CapSensor {
        cap: "cap_gpio_input_a",
        values: &["analog"],
        label: "cap_input_a",
        fields: &["input_a_voltage"],
    },
    CapSensor {
        cap: "cap_gpio_input_b",
        values: &["digital", "pulse"],
        label: "cap_input_b",
        fields: &["input_b_count"],
    },
    CapSensor {
        cap: "cap_gpio_input_b",
        values: &["analog"],
        label: "cap_input_b",
        fields: &["input_b_voltage"],
    },
];

/// Whether a cap sensor is on in a Node's config.
fn sensor_enabled(config: &serde_json::Map<String, serde_json::Value>, s: &CapSensor) -> bool {
    if s.values.is_empty() {
        return cap_enabled(config, s.cap);
    }
    match config.get(&format!("sensors.{}", s.cap)) {
        Some(serde_json::Value::String(v)) => s.values.iter().any(|w| v.eq_ignore_ascii_case(w)),
        _ => false,
    }
}

/// A 1-Wire slot type (firmware `enum app_w1_slot_type`, append-only; control-radio reports
/// it by name in `w1_slot_type`) and the quantities a slot of that type reports, as the
/// `w1_{N}_{quantity}` suffix. *Amended 2026-10-09 (v2.0.0, #119):* the names are the sensor
/// type registry's channels in snake case (`app_sensor_types.yaml`); the machine probe's
/// acceleration axes are left out as before.
pub(crate) struct W1Type {
    pub wire: &'static str,
    pub label: &'static str,
    pub part: &'static str,
    pub quantities: &'static [&'static str],
}

pub(crate) const W1_TYPES: &[W1Type] = &[
    W1Type {
        wire: "dallas",
        label: "Dallas",
        part: "DS18B20",
        quantities: &["temperature"],
    },
    W1Type {
        wire: "machine-probe",
        label: "machine probe",
        part: "DS28E17",
        quantities: &[
            "temperature",
            "humidity",
            "temperature_aux",
            "illuminance",
            "magnetic_field",
            "tilt",
        ],
    },
];

/// A 1-Wire quantity's display name, lowercase: it is the last part of
/// "1W slot N - <type> - <quantity>" (#122), which `field_label` translates and keeps lowercase.
fn w1_quantity_label(quantity: &str) -> &str {
    match quantity {
        "temperature" => "temperature",
        "humidity" => "humidity",
        "illuminance" => "illuminance",
        "magnetic_field" => "magnetic field",
        "temperature_aux" => "temperature (aux)",
        // `tilt_alert` is the v1.4.0 name of `tilt`, kept for the series a Node already sent.
        "tilt" | "tilt_alert" => "tilt",
        other => other,
    }
}

/// One quantity the Node measures, in display order. `label` overrides the registry's for a
/// 1-Wire field, which names the slot's type.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveField {
    pub name: String,
    pub label: Option<String>,
}

/// One occupied 1-Wire slot: its number (1-based, as the firmware and `sensorN` keys count),
/// its type and the fields it reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct W1SlotCtx {
    pub slot: usize,
    pub type_wire: String,
    pub type_label: String,
    pub part: Option<String>,
}

fn cap_enabled(config: &serde_json::Map<String, serde_json::Value>, cap: &str) -> bool {
    match config.get(&format!("sensors.{cap}")) {
        Some(serde_json::Value::Bool(on)) => *on,
        Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// Whether a config entry carries the Node's sensor declaration. The boot config info always
/// does (all nine `cap_*`), so its absence means the Node has not reported one yet.
fn declares_sensors(entry: &NodeConfigEntry) -> bool {
    entry.config.keys().any(|k| k.starts_with("sensors.cap_"))
}

/// The occupied 1-Wire slots, when `cap_w1_sensors` is on.
pub fn w1_slots(entry: &NodeConfigEntry) -> Vec<W1SlotCtx> {
    if !cap_enabled(&entry.config, "cap_w1_sensors") {
        return Vec::new();
    }
    entry
        .w1_slot_type
        .iter()
        .flatten()
        .enumerate()
        .filter(|(_, t)| t.as_str() != "empty")
        .map(|(i, t)| {
            let known = W1_TYPES.iter().find(|w| w.wire == t);
            W1SlotCtx {
                slot: i + 1,
                type_wire: t.clone(),
                type_label: known.map_or_else(|| t.clone(), |w| w.label.to_string()),
                part: known.map(|w| w.part.to_string()),
            }
        })
        .collect()
}

/// The quantities a Node measures, in display order: the board, then the `cap_*` sensors,
/// then the 1-Wire slots. Without config info (the Node has not booted since it was added)
/// the board and whatever has already arrived.
pub fn active_node_fields(config: Option<&NodeConfigEntry>, node: &NodeState) -> Vec<ActiveField> {
    if let Some(declared) = config.and_then(|e| declared_fields(e, Some(node))) {
        return declared;
    }
    let plain = |name: &str| ActiveField {
        name: name.to_string(),
        label: None,
    };
    let mut out: Vec<ActiveField> = BOARD_FIELDS.iter().map(|f| plain(f)).collect();
    for f in fiber_protocol::lorawan_registry() {
        // A switched-off sensor is still one the Node measures (GitLab #120).
        // A sensor sending `null` (on, no valid sample) is one too: its row reads "–".
        let reported = node.has_reading(f.name)
            || node.no_data_fields.contains(f.name)
            || node.disabled_fields.contains_key(f.name);
        if reported && !BOARD_FIELDS.contains(&f.name) && is_sensor_field(f) {
            out.push(plain(f.name));
        }
    }
    out
}

/// What the config info declares, or `None` when it declares no sensors (no config info yet).
/// A 1-Wire slot of a type this build does not know lists what that slot has reported in
/// `node`, and nothing without it.
pub fn declared_fields(
    entry: &NodeConfigEntry,
    node: Option<&NodeState>,
) -> Option<Vec<ActiveField>> {
    if !declares_sensors(entry) {
        return None;
    }
    let plain = |name: &str| ActiveField {
        name: name.to_string(),
        label: None,
    };
    let mut out: Vec<ActiveField> = BOARD_FIELDS.iter().map(|f| plain(f)).collect();
    for sensor in CAP_SENSORS {
        if sensor_enabled(&entry.config, sensor) {
            out.extend(sensor.fields.iter().map(|f| plain(f)));
        }
    }
    for slot in w1_slots(entry) {
        let prefix = format!("w1_{}_", slot.slot);
        let quantities: Vec<String> = match W1_TYPES.iter().find(|w| w.wire == slot.type_wire) {
            Some(w) => w.quantities.iter().map(|q| q.to_string()).collect(),
            None => node
                .map(|n| {
                    n.fields
                        .keys()
                        .chain(n.disabled_fields.keys())
                        .chain(n.no_data_fields.iter())
                        .filter_map(|k| k.strip_prefix(&prefix).map(str::to_string))
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect()
                })
                .unwrap_or_default(),
        };
        for q in quantities {
            out.push(ActiveField {
                name: format!("{prefix}{q}"),
                label: Some(format!(
                    "1W slot {} - {} - {}",
                    slot.slot,
                    slot.type_label,
                    w1_quantity_label(&q)
                )),
            });
        }
    }
    Some(out)
}

/// The Konfigurace card's History switch (devices.node.md §3.9, amendment 2026-09-25): one
/// switch for the Node keeping history and the portal downloading it after a reconnect.
#[derive(Debug, Clone, Serialize)]
pub struct NodeHistoryCtx {
    /// What the checkbox shows: the automatic download on, and the Node neither reporting nor
    /// drafted to keep no history.
    pub checked: bool,
    /// `application.history_enable` as the Node's config info reports it; `None` before one.
    pub node_enabled: Option<bool>,
    /// The draft's `application.history_enable`, while one waits to be sent.
    pub drafted: Option<bool>,
    /// `device_meta.backfill_auto`.
    pub backfill_auto: bool,
}

/// The config key the switch drafts, and the channel list it sends with it.
pub const HISTORY_ENABLE_KEY: &str = "application.history_enable";
/// v2.0.0 `history_channels` (`node-firmware-v2.0.0.md` §6), which replaces the v1.4.0
/// `history_sensors` mask: v2.0.0 has no mask to send.
pub const HISTORY_CHANNELS_KEY: &str = "application.history_channels";

pub fn node_history_ctx(
    config: Option<&NodeConfigEntry>,
    drafted: Option<bool>,
    backfill_auto: bool,
) -> NodeHistoryCtx {
    let node_enabled = config.and_then(|e| {
        e.config
            .get(HISTORY_ENABLE_KEY)
            .and_then(serde_json::Value::as_bool)
    });
    NodeHistoryCtx {
        checked: backfill_auto && drafted.or(node_enabled) != Some(false),
        node_enabled,
        drafted,
        backfill_auto,
    }
}

/// The `history_channels` list for what the Node measures (`node-firmware-v2.0.0.md` §6), as
/// the 48 hex characters `set-config` takes: one `slot << 5 | ch` column per recordable field
/// its config info declares (`declared_fields`), or of what it has reported when there is no
/// config info, the board's temperature and humidity always first, at most 24, `ff` after. A
/// 1-Wire field is recorded only when its slot type is known ([`SlotTypes::measured`]: the
/// configured type, else the detected one; a column names no type).
///
/// Only active sensors go in (amendment 2026-10-28, #160): a field switched off in the
/// Channels card (`NodeState::disabled_fields`) is left out, the board's included. `None` when
/// no column is left: there is nothing to record, and all-`ff` is not a list to send.
///
/// [`SlotTypes::measured`]: crate::radio::alarm::SlotTypes::measured
pub fn history_channels(
    entry: Option<&NodeConfigEntry>,
    node: Option<&NodeState>,
) -> Option<String> {
    use control_radio::decode::command_encode::HISTORY_CHANNEL_UNUSED;
    use control_radio::decode::history::history_column;
    const COLUMNS: usize = 24;
    let types = crate::radio::alarm::SlotTypes::measured(entry);
    let names: Vec<String> = match entry.and_then(|e| declared_fields(e, node)) {
        Some(fields) => fields.into_iter().map(|f| f.name).collect(),
        None => node
            .map(|n| n.fields.keys().chain(n.counters.keys()).cloned().collect())
            .unwrap_or_default(),
    };
    let mut columns: Vec<u8> = Vec::with_capacity(COLUMNS);
    for name in BOARD_FIELDS
        .iter()
        .copied()
        .chain(names.iter().map(String::as_str))
    {
        if node.is_some_and(|n| n.disabled_fields.contains_key(name)) {
            continue;
        }
        if let Some(c) = history_column(name, |slot| types.of(slot)) {
            if !columns.contains(&c) && columns.len() < COLUMNS {
                columns.push(c);
            }
        }
    }
    if columns.is_empty() {
        return None;
    }
    columns.resize(COLUMNS, HISTORY_CHANNEL_UNUSED);
    Some(columns.iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether two `history_channels` lists (48 hex characters) name the same columns, whatever
/// their order and padding (MR !129 review): the Node records the same set either way, so an
/// order the Node reports differently must not cost a resend and a restart. A list that is not
/// hex never equals another.
pub fn same_history_channels(a: &str, b: &str) -> bool {
    fn columns(list: &str) -> Option<std::collections::BTreeSet<u8>> {
        use control_radio::decode::command_encode::HISTORY_CHANNEL_UNUSED;
        if !list.len().is_multiple_of(2) || !list.is_ascii() {
            return None;
        }
        (0..list.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&list[i..i + 2], 16).ok())
            .filter(|c| *c != Some(HISTORY_CHANNEL_UNUSED))
            .collect()
    }
    matches!((columns(a), columns(b)), (Some(x), Some(y)) if x == y)
}

/// The Diagnostics list of the Node's active sensors (devices.node.md §3.2, WP3).
#[derive(Debug, Clone, Serialize)]
pub struct ActiveSensorsCtx {
    /// The enabled `cap_*` sensors, as their config key (`cap_barometer`); the template names
    /// them through i18n (`monitoring.sensor.<cap>`). The board is always there and is not
    /// listed here.
    pub caps: Vec<String>,
    pub w1_slots: Vec<W1SlotCtx>,
    /// When the config info the list comes from arrived, RFC3339.
    pub synced_at: Option<String>,
}

pub fn active_sensors_ctx(config: Option<&NodeConfigEntry>) -> Option<ActiveSensorsCtx> {
    let entry = config.filter(|e| declares_sensors(e))?;
    Some(ActiveSensorsCtx {
        caps: CAP_SENSORS
            .iter()
            .filter(|s| sensor_enabled(&entry.config, s))
            .map(|s| s.label.to_string())
            .collect(),
        w1_slots: w1_slots(entry),
        synced_at: entry.synced_at.clone(),
    })
}

/// The DeviceInfo numbers the card cannot show as they arrive: the Node
/// reports `battery` in mV, and `device_status`/`reset_cause` as bitmasks
/// (`app_config.proto` `Response.Info` fields 10, 11, 14). Each field is
/// `None` when the document does not carry it as a number, and the template
/// then falls back to the raw value, so a v1 document keeps rendering the way
/// it did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeviceInfoView {
    /// `x.yy V` (devices.node.md §2.6); `"–"` for the firmware's
    /// "0 = unavailable".
    pub battery: Option<String>,
    /// Active `APP_DEVICE_STATUS_*` flags; empty is "all OK".
    pub status_flags: Option<Vec<String>>,
    /// Zephyr hwinfo `RESET_*` flags of the last boot; empty is "unknown".
    pub reset_causes: Option<Vec<String>>,
    /// `uptime_s` as an operator reads it: `18 s`, `1 h 50 min`, `3 d 4 h`.
    pub uptime: Option<String>,
    /// The Node's clock when it reported, against the time the Hub received it
    /// (nodes_phase1.cs.md WP7). `None` when the document carries no `unix_time`.
    pub clock: Option<NodeClockCtx>,
    /// `Info.active_alarms` by name; empty is "none". `None` without the key (v1).
    pub active_alarms: Option<Vec<ActiveAlarmCtx>>,
    /// `Info.w1_slot_state` of slots 1..4 (`none` | `ok` | `absent` | `replaced` | `mismatch`,
    /// v2.0.0). `None` before v2.0.0.
    pub w1_slot_state: Option<Vec<String>>,
}

/// The Node's clock (`Info.unix_time`).
#[derive(Debug, Clone, Serialize)]
pub struct NodeClockCtx {
    /// `false` when the Node has no wall clock yet (`time_unsynced`, or a time before 2020).
    pub synced: bool,
    /// RFC 3339, for `short_dt`.
    pub node_time: Option<String>,
    /// Node minus Hub receive time, seconds; `None` without a receive time.
    pub offset_s: Option<i64>,
}

/// One `Info.active_alarms` entry by name (sticker-firmware `app_alarm_rules.h`
/// `app_alarm_source` / `app_alarm_quantity`, `AlarmEvent.Type`). A value the tables do not
/// know reads as its number. From v2.0.0 the entry names a registry channel instead.
#[derive(Debug, Clone, Serialize)]
pub struct ActiveAlarmCtx {
    /// `onboard`, `slot`, `hall_left`, …
    pub source: String,
    /// The 1-Wire slot, 1..4, when `source` is `slot`.
    pub slot: Option<u64>,
    pub quantity: String,
    /// `low` | `high` | `trigger` | `no_data`.
    pub kind: String,
}

/// `AlarmEvent.Type`; 0 is `TYPE_NONE`, never an active alarm.
const ALARM_TYPES: &[&str] = &["none", "low", "high", "trigger", "no_data"];

/// The first Unix time the portal believes: the firmware counts from 0 until it is synced.
const NODE_CLOCK_EPOCH: i64 = 1_577_836_800; // 2020-01-01

/// Seconds as the two largest units: `18 s`, `2 min 5 s`, `1 h 50 min`, `3 d 4 h`.
fn uptime_text(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s} s"),
        (0, 0, _) => format!("{m} min {s} s"),
        (0, _, _) => format!("{h} h {m} min"),
        _ => format!("{d} d {h} h"),
    }
}

fn active_alarm_ctx(entry: &serde_json::Value) -> ActiveAlarmCtx {
    let num = |k: &str| entry.get(k).and_then(serde_json::Value::as_u64);
    // v2.0.0 (node-firmware-v2.0.0.md §4): `{slot, channel, type, sensor_type?}`, the target a
    // registry channel. The card names it by the channel's quantity, a mismatch by the type the
    // slot expected.
    if entry.get("channel").is_some() {
        use control_radio::decode::sensor_types;
        let slot = num("slot").unwrap_or(0);
        let small = |v: Option<u64>| v.and_then(|v| u8::try_from(v).ok());
        let kind = entry
            .get("type_name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                num("type").map_or_else(String::new, |t| {
                    control_radio::decode::node::ALARM_TYPES
                        .get(t as usize)
                        .map_or_else(|| t.to_string(), |n| (*n).to_string())
                })
            });
        let quantity = if kind == "sensor_mismatch" {
            entry
                .get("sensor_type_name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .unwrap_or_default()
        } else {
            let ch = small(num("channel"));
            small(Some(slot))
                .zip(ch)
                .and_then(|(s, ch)| sensor_types::channel(s, small(num("sensor_type")), ch))
                .map(|c| c.quantity.to_string())
                .unwrap_or_else(|| ch.map_or_else(String::new, |c| format!("ch{c}")))
        };
        return ActiveAlarmCtx {
            source: if slot == 0 { "onboard" } else { "slot" }.to_string(),
            slot: (1..=4).contains(&slot).then_some(slot),
            quantity,
            kind,
        };
    }
    // Pre-2.0.0 `{source, quantity}` entries are not supported: the firmware is gated.
    ActiveAlarmCtx {
        source: String::new(),
        slot: None,
        quantity: String::new(),
        kind: num("type")
            .and_then(|t| ALARM_TYPES.get(t as usize))
            .map(|n| (*n).to_string())
            .unwrap_or_default(),
    }
}

impl DeviceInfoView {
    fn of(info: &serde_json::Value, received_at: Option<&str>) -> Self {
        let num = |k: &str| info.get(k).and_then(serde_json::Value::as_u64);
        let mask = |k: &str| num(k).and_then(|v| u32::try_from(v).ok());
        // A decoded fPort-85 Info (control-radio always derives `fw_major`) is proto3: a zero
        // scalar never reaches the wire, so an absent mask IS 0 — "all OK", not "unknown"
        // (WP4 on 0413: the card had no status row at all).
        let proto3 = info.get("fw_major").is_some();
        let unsynced_bit = DEVICE_STATUS_BITS
            .iter()
            .find(|(_, name)| *name == "time_unsynced")
            .map(|(bit, _)| 1u32 << bit)
            .unwrap_or(0);
        let clock = num("unix_time")
            .and_then(|t| i64::try_from(t).ok())
            .map(|node| {
                let synced = node >= NODE_CLOCK_EPOCH
                    && mask("device_status").is_none_or(|m| m & unsynced_bit == 0);
                let hub = received_at.and_then(|at| {
                    time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
                        .ok()
                        .map(time::OffsetDateTime::unix_timestamp)
                });
                NodeClockCtx {
                    synced,
                    node_time: synced
                        .then(|| crate::render::unix_to_rfc3339(node))
                        .flatten(),
                    offset_s: synced.then_some(hub.map(|h| node - h)).flatten(),
                }
            });
        Self {
            uptime: num("uptime_s").map(uptime_text),
            clock,
            w1_slot_state: info
                .get("w1_slot_state")
                .and_then(serde_json::Value::as_array)
                .map(|states| {
                    states
                        .iter()
                        .map(|v| match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect()
                }),
            active_alarms: info
                .get("active_alarms")
                .and_then(serde_json::Value::as_array)
                .map(|alarms| alarms.iter().map(active_alarm_ctx).collect()),
            // control-radio publishes the firmware's "0 = unavailable" as `null`.
            battery: match info.get("battery") {
                Some(serde_json::Value::Null) => Some("–".to_string()),
                Some(v) => v.as_u64().map(|mv| match mv {
                    0 => "–".to_string(),
                    mv => format!("{:.2} V", mv as f64 / 1000.0),
                }),
                None => None,
            },
            status_flags: mask("device_status")
                .or(proto3.then_some(0))
                .map(|v| flag_names(v, DEVICE_STATUS_BITS)),
            reset_causes: mask("reset_cause")
                .or(proto3.then_some(0))
                .map(|v| flag_names(v, RESET_CAUSE_BITS)),
        }
    }
}

/// `APP_DEVICE_STATUS_*` of sticker-firmware v2.0.0 (`app_cmd.h`, layout re-grouped for v2.0.0
/// in #415 and declared stable from then on), the only release the portal targets
/// (nodes_phase1.cs.md Q12). Bits 6-7, 11 and 18-31 are reserved.
const DEVICE_STATUS_BITS: &[(u32, &str)] = &[
    (0, "alarm_any"),
    (1, "alarm_threshold"),
    (2, "alarm_state"),
    (3, "alarm_rate"),
    (4, "alarm_no_data"),
    (5, "alarm_low_batt"),
    (8, "radio_off"),
    (9, "lrw_disabled"),
    (10, "radio_link_down"),
    (12, "nfc_down"),
    (13, "mailbox_down"),
    (14, "i2c_wedged"),
    (15, "history_down"),
    (16, "time_unsynced"),
    (17, "claim_active"),
];

/// Zephyr `drivers/hwinfo.h` `RESET_*`.
const RESET_CAUSE_BITS: &[(u32, &str)] = &[
    (0, "pin"),
    (1, "software"),
    (2, "brownout"),
    (3, "power_on"),
    (4, "watchdog"),
    (5, "debug"),
    (6, "security"),
    (7, "low_power_wake"),
    (8, "cpu_lockup"),
    (9, "parity"),
    (10, "pll"),
    (11, "clock"),
    (12, "hardware"),
    (13, "user"),
    (14, "temperature"),
    (15, "bootloader"),
    (16, "flash"),
];

/// Names the set bits of `mask`, lowest first. A bit the table does not know
/// reads `bitN` rather than being dropped, so a firmware that adds one
/// cannot go unnoticed.
fn flag_names(mask: u32, table: &[(u32, &str)]) -> Vec<String> {
    (0..32)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| match table.iter().find(|(b, _)| *b == bit) {
            Some((_, name)) => (*name).to_string(),
            None => format!("bit{bit}"),
        })
        .collect()
}

/// Whether a Node has been registered but has never been heard from
/// (devices.node.md §5, "registered — waiting for join").
///
/// **Derived, not stored.** B4's plan considered a `pending_join` column;
/// there is nothing to store, because both ingest arms already say it:
///
/// * the v1 `config/state` arm inserts a `NodeState::new` whose
///   `alarm_state` is `NeverConnected` and whose `last_seen` is `None`
///   until a frame arrives;
/// * the v2 `NodeInventory` arm carries `info.pairing_status`, which is
///   `null` until the first uplink flips it to `"joined"`.
///
/// So `last_seen.is_none()` covers the v1 arm and the `pairing_status`
/// clause covers the v2 one, where a Node can hold a stale `last_seen` from
/// an earlier enrolment while its current pairing has not completed. Both
/// have to be true: a Node that has reported is joined whatever the
/// inventory says, and a Node the Hub calls `"joined"` is not waiting.
///
/// **`has_history` is the restart clause** (§21 review, item 8).
/// `NodeState::last_seen` is in-memory only: after a portal restart it is
/// `None` for every Node until the next uplink, so a fleet of perfectly
/// ordinary Nodes on a slow cadence all read as "registered — waiting for
/// join" for as long as they stay quiet, and a genuinely dead one is
/// indistinguishable from a brand-new registration. The portal's own stored
/// readings are the memory the store lost: a Node it has ever recorded a
/// reading for has demonstrably joined, whatever this process happens to
/// remember. The list resolves it in bulk (`NodeRowData::has_readings`),
/// never per row — §4's performance gate.
///
/// This is a **lifecycle** state, not a fault. A Node waiting for its join
/// is doing exactly what it should; `node_fault` deliberately does not
/// report it (a fresh registration would otherwise light the fault
/// indicator on the whole list until the first uplink).
pub fn waiting_for_join(
    node: &NodeState,
    info: Option<&serde_json::Value>,
    has_history: bool,
) -> bool {
    if node.last_seen.is_some() || has_history {
        return false;
    }
    let joined = info
        .and_then(|i| i.get("pairing_status"))
        .and_then(serde_json::Value::as_str)
        == Some("joined");
    !joined
}

// ---------------------------------------------------------------------
// WP5: the Configuration tab (nodes_phase1.cs.md WP5, devices.node.md §3.6)
// ---------------------------------------------------------------------

/// The only config keys the portal edits. The rest of config info is read-only here; the
/// service card's raw message reaches the other writable keys (WP6).
///
/// *Amended 2026-10-14 (devices.node.md dodatek 2026-10-14, upřesněno):* the three GP pin
/// enums stay read-only here (Hynek); the card shows their translated option label.
pub const EDITABLE_CONFIG_KEYS: [&str; 2] =
    ["application.interval_report", "application.interval_sample"];

/// The GP pin enums (v2.0.0) whose value the card shows as a translated option label.
const LABELLED_ENUM_KEYS: [&str; 3] = [
    "sensors.cap_gpio_mode",
    "sensors.cap_gpio_input_a",
    "sensors.cap_gpio_input_b",
];

/// The Node's settings-info: what it sends by itself after every boot and in answer to
/// GetSettings (sticker-firmware #412/#428, `app_cmd.c` `cs_app_ids` and `cs_sensor_ids`), in
/// the firmware's order. The Configuration card shows these keys and nothing else: the Hub also
/// holds keys from a full `get-config` and from acked writes, but it never learns when one of
/// those changes over the shell or NFC, so showing them would show stale values (N11,
/// nodes_phase1.cs.md WP5). The 1-Wire slot types ride the same dump, beside `config`.
pub const SETTINGS_INFO_KEYS: [&str; 13] = [
    "application.interval_sample",
    "application.interval_report",
    "application.history_enable",
    "sensors.cap_hall_left",
    "sensors.cap_hall_right",
    "sensors.cap_light_sensor",
    "sensors.cap_barometer",
    "sensors.cap_w1_sensors",
    "sensors.cap_accelerometer",
    "sensors.cap_sht",
    "sensors.cap_gpio_mode",
    "sensors.cap_gpio_input_a",
    "sensors.cap_gpio_input_b",
];

/// What an editable key's input takes, from the firmware's registry.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigInputCtx {
    pub min: Option<i64>,
    pub max: Option<i64>,
    /// `0` is accepted beside the range (`interval_sample`: 0 = sample on report).
    pub zero_allowed: bool,
    /// `60–86400`, `0 or 5–3600`.
    pub range: Option<String>,
    pub default: Option<String>,
    /// An enum key's values in the firmware's shell spelling, for a select; empty for an
    /// integer key.
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigRowCtx {
    /// `group.field`, the storage and wire spelling.
    pub key: String,
    /// The name after the dot.
    pub name: String,
    pub value: String,
    /// `Some` for the keys the portal edits.
    pub edit: Option<ConfigInputCtx>,
    /// The i18n key of a GP pin enum's value label, `None` for a raw value.
    pub value_label: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigGroupCtx {
    pub id: String,
    pub rows: Vec<ConfigRowCtx>,
}

/// The Configuration card's context, for the page and for its SSE repaint alike.
#[derive(Debug, Clone, Serialize)]
pub struct NodeConfigCtx {
    /// Config info by group, read-only rows and the editable ones. `None` = never read.
    pub groups: Option<Vec<ConfigGroupCtx>>,
    /// The editable rows again, flat, for the edit dialogs outside the swap region.
    pub editable: Vec<ConfigRowCtx>,
    pub read_status: Option<String>,
    pub synced_at: Option<String>,
    pub missing: Vec<String>,
    /// The 1-Wire slot types from the Node's boot `ConfigDump`, read-only
    /// (node_reported_state.md §4). `None` = never reported.
    pub w1_slot_type: Option<Vec<String>>,
}

/// The option label key of a GP pin enum's value, if the catalogue has one.
fn config_value_label(key: &str, value: &serde_json::Value) -> Option<String> {
    if !LABELLED_ENUM_KEYS.contains(&key) {
        return None;
    }
    let (_, name) = key.split_once('.')?;
    let label = format!(
        "monitoring.node_detail.config_option_{name}_{}",
        value.as_str()?.to_ascii_lowercase()
    );
    (crate::i18n::lookup("en", &label) != label).then_some(label)
}

/// The input an editable key gets, `None` for every other key.
pub fn config_input(key: &str) -> Option<ConfigInputCtx> {
    use control_radio::decode::node_params::{node_param, ParamKind};
    if !EDITABLE_CONFIG_KEYS.contains(&key) {
        return None;
    }
    let param = node_param(key)?;
    match param.kind {
        ParamKind::Int {
            min,
            max,
            zero_allowed,
        } => Some(ConfigInputCtx {
            min,
            max,
            zero_allowed,
            range: param.range_text(),
            default: param.default.clone(),
            options: Vec::new(),
        }),
        ParamKind::Enum(ref values) => Some(ConfigInputCtx {
            min: None,
            max: None,
            zero_allowed: false,
            range: None,
            default: param.default.clone(),
            options: values.iter().map(|(name, _)| name.clone()).collect(),
        }),
        _ => None,
    }
}

/// A config value as the one line the card shows.
fn config_value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "–".to_string(),
        other => other.to_string(),
    }
}

/// Builds the Configuration card from the Node's cached config info.
pub fn node_config_ctx(entry: Option<&NodeConfigEntry>) -> NodeConfigCtx {
    let Some(entry) = entry else {
        return NodeConfigCtx {
            groups: None,
            editable: Vec::new(),
            read_status: None,
            synced_at: None,
            missing: Vec::new(),
            w1_slot_type: None,
        };
    };
    // The settings-info keys the Node has reported, in the firmware's order, by group.
    let mut groups: Vec<ConfigGroupCtx> = Vec::new();
    for key in SETTINGS_INFO_KEYS {
        let Some(value) = entry.config.get(key) else {
            continue;
        };
        let (group, name) = key.split_once('.').unwrap_or(("", key));
        let row = ConfigRowCtx {
            key: key.to_string(),
            name: name.to_string(),
            value: config_value_text(value),
            edit: config_input(key),
            value_label: config_value_label(key, value),
        };
        match groups.last_mut() {
            Some(last) if last.id == group => last.rows.push(row),
            _ => groups.push(ConfigGroupCtx {
                id: group.to_string(),
                rows: vec![row],
            }),
        }
    }
    let editable = groups
        .iter()
        .flat_map(|g| g.rows.iter().filter(|r| r.edit.is_some()).cloned())
        .collect();
    NodeConfigCtx {
        groups: Some(groups),
        editable,
        read_status: Some(entry.read_status.clone()),
        synced_at: entry.synced_at.clone(),
        missing: entry.missing.clone(),
        w1_slot_type: entry.w1_slot_type.clone(),
    }
}

// ---------------------------------------------------------------------
// WP6: the service card's raw message (nodes_phase1.cs.md WP6)
// ---------------------------------------------------------------------

/// The groups a raw message offers templates for, in this order.
const RAW_MESSAGE_GROUPS: [&str; 3] = ["application", "sensors", "alarms"];

/// Whether the raw message refuses `key`: the alarm slots (the portal owns the table, WP2),
/// `history_sensors` (NFC only, Q11) and its v2.0.0 successor `history_channels` (the History
/// switch owns it), the 1-Wire ROM codes, the LoRaWAN group (NFC and shell
/// only) and `vendor_reset_allow`. Everything else the firmware answers for itself.
pub fn raw_message_key_refused(key: &str) -> bool {
    let Some((group, name)) = key.split_once('.') else {
        return false;
    };
    let numbered = |prefix: &str, suffix: &str| {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(suffix))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    group == "lorawan"
        || (group == "alarms" && numbered("alarm_", ""))
        || (group == "sensors" && numbered("sensor", "_rom"))
        || key == "application.history_sensors"
        || key == HISTORY_CHANNELS_KEY
        || key == "application.vendor_reset_allow"
}

/// One key of a raw-message template, as the help table under the editor shows it.
#[derive(Debug, Clone, Serialize)]
pub struct RawKeyCtx {
    pub key: String,
    /// `bool`, `60–86400`, `off | once | …`.
    pub takes: String,
    pub help: Option<String>,
}

/// A `set-config` JSON template for one group, filled with the Node's current values.
#[derive(Debug, Clone, Serialize)]
pub struct RawTemplateCtx {
    pub group: String,
    /// Pretty-printed JSON object, ready for the editor.
    pub json: String,
    pub keys: Vec<RawKeyCtx>,
}

/// The raw message's templates from the firmware registry: every key writable over the air on
/// both carriers (`writable_over_the_air`: one template for every Node, whichever carrier it is
/// on) that control-radio can encode and the raw message does not refuse, valued from the Node's
/// settings-info where it reports the key, else the firmware default.
pub fn raw_message_templates(config: Option<&NodeConfigEntry>) -> Vec<RawTemplateCtx> {
    use control_radio::decode::command_encode::CONFIG_KEYS;
    use control_radio::decode::node_params::{node_params, ParamKind};
    RAW_MESSAGE_GROUPS
        .iter()
        .filter_map(|group| {
            let mut json = serde_json::Map::new();
            let mut keys = Vec::new();
            for param in node_params().iter().filter(|p| p.group == *group) {
                let key = param.full_name();
                if !param.writable_over_the_air()
                    || raw_message_key_refused(&key)
                    || !CONFIG_KEYS.iter().any(|k| k.full_name() == key)
                {
                    continue;
                }
                // The Node's value only where its settings-info reports it; a key known
                // only from an older full read may be stale, so the default stands in.
                let current = SETTINGS_INFO_KEYS
                    .contains(&key.as_str())
                    .then(|| config.and_then(|c| c.config.get(&key)).cloned())
                    .flatten();
                let fallback = || -> serde_json::Value {
                    let default = param.default.as_deref();
                    match &param.kind {
                        ParamKind::Bool => serde_json::Value::Bool(default == Some("true")),
                        ParamKind::Int { .. } => default
                            .and_then(|d| d.parse::<i64>().ok())
                            .map(serde_json::Value::from)
                            .unwrap_or(serde_json::Value::from(0)),
                        ParamKind::Enum(values) => serde_json::Value::String(
                            default
                                .map(str::to_string)
                                .or_else(|| values.first().map(|(v, _)| v.clone()))
                                .unwrap_or_default(),
                        ),
                        ParamKind::Bytes => serde_json::Value::String(String::new()),
                    }
                };
                json.insert(key.clone(), current.unwrap_or_else(fallback));
                let takes = match &param.kind {
                    ParamKind::Bool => "true | false".to_string(),
                    ParamKind::Int { .. } => param.range_text().unwrap_or_else(|| "int".into()),
                    ParamKind::Enum(values) => values
                        .iter()
                        .map(|(v, _)| v.as_str())
                        .filter(|v| !v.starts_with("reserved"))
                        .collect::<Vec<_>>()
                        .join(" | "),
                    ParamKind::Bytes => "hex".to_string(),
                };
                keys.push(RawKeyCtx {
                    key,
                    takes,
                    help: param.help.clone(),
                });
            }
            (!keys.is_empty()).then(|| RawTemplateCtx {
                group: group.to_string(),
                json: serde_json::to_string_pretty(&serde_json::Value::Object(json))
                    .unwrap_or_default(),
                keys,
            })
        })
        .collect()
}

/// The Force send confirmation's state (nodes_phase1.cs.md WP6): the firmware has no fPort-85
/// answer, so the first fPort-2 reading after the send confirms it, within two report
/// intervals.
#[derive(Debug, Clone, Serialize)]
pub struct ForceSendCtx {
    pub node_key: String,
    /// `waiting` | `confirmed` | `no_response` | `error`.
    pub state: &'static str,
    pub since: i64,
    pub until: i64,
    /// RFC 3339, for `short_dt`.
    pub sent_at: Option<String>,
    pub until_at: Option<String>,
    pub reading_at: Option<String>,
    pub error: Option<String>,
}

impl ForceSendCtx {
    pub fn error(node_key: &str, message: String) -> Self {
        Self {
            node_key: node_key.to_string(),
            state: "error",
            since: 0,
            until: 0,
            sent_at: None,
            until_at: None,
            reading_at: None,
            error: Some(message),
        }
    }

    /// Where a send made at `since` stands at `now`, given the Node's newest reading at or
    /// after it.
    pub fn at(node_key: &str, since: i64, until: i64, reading: Option<i64>, now: i64) -> Self {
        let state = match reading {
            Some(_) => "confirmed",
            None if now > until => "no_response",
            None => "waiting",
        };
        Self {
            node_key: node_key.to_string(),
            state,
            since,
            until,
            sent_at: crate::render::unix_to_rfc3339(since),
            until_at: crate::render::unix_to_rfc3339(until),
            reading_at: reading.and_then(crate::render::unix_to_rfc3339),
            error: None,
        }
    }
}

// ---------------------------------------------------------------------
// B4: the pending-configuration draft (devices.node.md §3.5)
// ---------------------------------------------------------------------

/// One outstanding key in a Node's draft, as `partials/pending_config.html`
/// reads it.
#[derive(Debug, Clone, Serialize)]
pub struct PendingRowCtx {
    /// The storage key — `application.interval_report`, or
    /// `threshold.{field}.{bound}`.
    pub param_key: String,
    /// What the table shows in the key column. A config key shows itself; a
    /// threshold key is split back into "{field} · {bound}" so the operator
    /// reads the thing they edited rather than the storage encoding.
    pub label: String,
    pub kind: String,
    /// The drafted value, as text. JSON scalars are unwrapped (`"600"`, not
    /// `"600"` with quotes; `null` becomes the em dash the table uses for
    /// "unset").
    pub desired: String,
    /// What the Node holds now, when the portal knows — the device's own
    /// config pages for a config key, the merged thresholds for a bound.
    /// `None` renders as "unknown", which is honest: a Node whose config
    /// has never been read has no current value the portal can quote.
    pub current: Option<String>,
    pub author: String,
    /// `HH:MM:SS`, same convention as `command_status`'s `ts`.
    pub changed_at: String,
    /// One of `portal_storage::PENDING_STATES`; the template renders
    /// `monitoring.pending.state_{state}`.
    pub state: String,
    pub last_error: Option<String>,
    /// The Hub and the portal already hold this row's value; only the Node does not (PF-3).
    /// `current` then quotes what the Node holds, the value from before the send.
    pub hub_set: bool,
    /// This row says `sending` in the database but has said so for longer
    /// than any walk can take, so the banner reports it as an interrupted
    /// send instead (§21 review, defect 2). Rendered as `state = "error"`
    /// with `monitoring.pending.send_interrupted` — see
    /// [`crate::pending::SENDING_STALE_SLACK`] for why the banner has to
    /// have an opinion of its own here.
    pub stale: bool,
}

/// A Node's whole draft — the banner's context.
///
/// Always built, even when empty: `partials/pending_config.html` is an
/// always-present SSE swap shell (same rule as `command_status.html`), so
/// the fragment has to render to *something* when there is no draft.
#[derive(Debug, Clone, Serialize)]
pub struct PendingCtx {
    pub node_key: String,
    pub rows: Vec<PendingRowCtx>,
    /// Manager+; gates Send and Discard. A viewer sees the banner — the
    /// draft is shared state and hiding it would make a colleague's pending
    /// change invisible — and cannot act on it.
    pub can_edit: bool,
    /// A send is walking this draft right now: Send and Discard are
    /// disabled and the row in flight shows its spinner.
    pub sending: bool,
    /// At least one row failed — an `error`, or a config key the Node did not
    /// confirm (PF-3). The banner turns red.
    pub failed: bool,
    /// Something is `error` or `undelivered`: the banner offers "send again"
    /// rather than "send" (PF-3).
    pub resend: bool,
    /// A bound is in effect (Hub and portal) but not on the Node yet (PF-3).
    /// Not a failure — telemetry evaluation is the primary alarm path — so the
    /// banner says so without turning red.
    pub undelivered_bounds: bool,
    /// The portal drafted rows itself because the Node's active sensors moved its alarm slots
    /// (nodes_phase1.cs.md WP2); the banner says so.
    pub alarm_layout_moved: bool,
    /// The portal drafted its own bounds again because the Node took a discarded band (PF-3b);
    /// the banner says why they are there.
    pub node_differs: bool,
    /// fPort-85 frames Send puts on the air at the Node's last data rate; each ends with the
    /// Node saving and restarting. `None` = nothing goes to the Node (a warning-only edit) or
    /// the plan failed, which the send then reports itself (nodes_phase1.cs.md WP5).
    pub downlinks: Option<usize>,
    /// How many minutes a send waits for the Node's ack: it fetches the downlink at its next
    /// uplink (N13). The banner says so while a send is in flight.
    pub confirm_minutes: u64,
    /// Send would refuse this draft ([`crate::pending::refusal`]): the banner says why before
    /// anyone presses it, and Send is disabled (#126 item 5). `None` from [`pending_ctx`]; the
    /// route that plans the send fills it.
    pub refusal: Option<SendRefusalCtx>,
    /// The Node was sent its settings and has not confirmed them yet (config
    /// sync Q20c): when, how often, and whether the 4 h passed. `None` from
    /// [`pending_ctx`]; the route fills it from the deadline store.
    pub awaiting: Option<AwaitingCtx>,
}

/// The "waiting for settings" part of the draft banner (config sync Q20c). No countdown:
/// the banner says when it was sent and how often, and only whether the 4 h passed
/// (Hynek 2026-10-15); the deadline itself lives in the cfgsync fault.
#[derive(Debug, Clone, Serialize)]
pub struct AwaitingCtx {
    pub sent_at: String,
    pub attempts: i64,
    pub passed: bool,
}

impl AwaitingCtx {
    pub fn of(d: &portal_storage::NodeConfigDeadline, now: i64) -> Self {
        Self {
            sent_at: format_time(d.sent_at),
            attempts: d.attempts,
            passed: d.deadline_at <= now,
        }
    }
}

/// [`crate::pending::SendRefusal`] for the draft banner: the message's i18n key and its
/// placeholders, rendered in the viewer's language by the template.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SendRefusalCtx {
    pub key: String,
    pub fields: String,
    pub keys: String,
    pub version: String,
}

impl SendRefusalCtx {
    pub fn of(refusal: &crate::pending::SendRefusal) -> Self {
        let (key, args) = refusal.parts();
        let arg = |name: &str| {
            args.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        Self {
            key: key.to_string(),
            fields: arg("{fields}"),
            keys: arg("{keys}"),
            version: arg("{version}"),
        }
    }
}

/// "On the Node" for one field of the thresholds table (PF-3, nodes_phase1.cs.md dodatek
/// 2026-09-27). Secondary information beside the bound: the bound is evaluated from telemetry
/// whatever this says.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NodeDeliveryCtx {
    /// `yes` (the Node confirmed this very band, `at`), `no` (it holds another one),
    /// `pending` (on its way), `undelivered` (did not land in time), `unknown` (the portal has
    /// not seen the Node confirm a rule for it), `unknown_maybe` (a Discard took back a frame
    /// the Node may have had, PF-3b), `differs` (the Node took a discarded band, PF-3b), or
    /// `hub_only` (nothing of it lives on the Node: no slot, or warning bounds only).
    pub state: &'static str,
    /// RFC 3339 of the confirmation, for `short_dt`.
    pub at: Option<String>,
    /// config_sync.md §13 Q20e: the Node confirmed another critical band than the portal
    /// holds and nothing is pending — the "not synchronised" badge, naming the Node's band.
    pub unsynced: Option<NodeUnsyncedCtx>,
}

/// The band a Node holds where it differs from the portal's (config_sync.md §13 Q20e).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NodeUnsyncedCtx {
    /// The Node's critical band in displayed units, `low – high` (`–` for no bound).
    pub band: String,
    /// The Node holds the band switched off.
    pub off: bool,
}

/// Whether one limit is not synchronised with the Node (config_sync.md §13 Q20e): the Node
/// confirmed a critical band (`record`) other than the one the portal holds (`t`, in displayed
/// units). `None` for a field the Node never confirmed (no band to name), one on its way or in
/// doubt, and one with no critical bound in the portal. The caller checks nothing is pending.
pub fn node_unsynced(
    field: &str,
    t: &ThresholdEntryCtx,
    record: Option<&portal_storage::NodeThresholdDelivery>,
) -> Option<NodeUnsyncedCtx> {
    let d = record?;
    if d.confirmed_at.is_none()
        || !(d.state == portal_storage::DELIVERY_STATE_CONFIRMED
            || d.state == portal_storage::DELIVERY_STATE_DIFFERS)
    {
        return None;
    }
    if t.critical_low.is_none() && t.critical_high.is_none() {
        return None;
    }
    let shown = |v: Option<f64>| match fiber_protocol::lorawan_field(field) {
        Some(f) => v.map(|v| f.to_display(v)),
        None => v,
    };
    let (low, high) = (shown(d.confirmed_low), shown(d.confirmed_high));
    let same = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() < 1e-6,
        (None, None) => true,
        _ => false,
    };
    let enabled = d.confirmed_enabled.unwrap_or(true);
    if same(low, t.critical_low) && same(high, t.critical_high) && enabled == t.enabled {
        return None;
    }
    let fmt = |v: Option<f64>| match v {
        Some(v) => {
            let r = (v * 100.0).round() / 100.0;
            format!("{r}")
        }
        None => "–".to_string(),
    };
    Some(NodeUnsyncedCtx {
        band: format!("{} – {}", fmt(low), fmt(high)),
        off: !enabled,
    })
}

/// The thresholds table's "on the Node" column: one entry per field the portal holds a band
/// for. Only a slotted field's critical band and switch live on the Node (WP2).
pub fn node_delivery_ctx(
    thresholds: &BTreeMap<String, ThresholdEntryCtx>,
    alarm_layout: &[String],
    delivery: &[portal_storage::NodeThresholdDelivery],
    nothing_pending: bool,
) -> BTreeMap<String, NodeDeliveryCtx> {
    thresholds
        .iter()
        .map(|(field, t)| {
            let slotted = alarm_layout.contains(field)
                && (t.critical_low.is_some() || t.critical_high.is_some());
            let record = delivery.iter().find(|d| &d.field == field);
            let band = portal_storage::NodeBand {
                field: field.clone(),
                low: t.critical_low,
                high: t.critical_high,
                enabled: t.enabled,
            };
            let ctx = match record {
                _ if !slotted => NodeDeliveryCtx {
                    state: "hub_only",
                    at: None,
                    unsynced: None,
                },
                None => NodeDeliveryCtx {
                    state: "unknown",
                    at: None,
                    unsynced: None,
                },
                Some(d) if d.state == portal_storage::DELIVERY_STATE_PENDING => NodeDeliveryCtx {
                    state: "pending",
                    at: None,
                    unsynced: None,
                },
                Some(d) if d.state == portal_storage::DELIVERY_STATE_UNDELIVERED => {
                    NodeDeliveryCtx {
                        state: "undelivered",
                        at: None,
                        unsynced: None,
                    }
                }
                // PF-3b: a Discard took back a frame the Node may have had.
                Some(d) if d.state == portal_storage::DELIVERY_STATE_UNKNOWN => NodeDeliveryCtx {
                    state: "unknown_maybe",
                    at: None,
                    unsynced: None,
                },
                // PF-3b: the Node answered a discarded frame and holds that band.
                Some(d) if d.state == portal_storage::DELIVERY_STATE_DIFFERS => NodeDeliveryCtx {
                    state: "differs",
                    at: d.confirmed_at.and_then(super::unix_to_rfc3339),
                    unsynced: None,
                },
                Some(d) if d.holds(&band) => NodeDeliveryCtx {
                    state: "yes",
                    at: d.confirmed_at.and_then(super::unix_to_rfc3339),
                    unsynced: None,
                },
                Some(_) => NodeDeliveryCtx {
                    state: "no",
                    at: None,
                    unsynced: None,
                },
            };
            let unsynced = if nothing_pending && slotted {
                node_unsynced(field, t, record)
            } else {
                None
            };
            (field.clone(), NodeDeliveryCtx { unsynced, ..ctx })
        })
        .collect()
}

/// Splits a `param_key` into what the banner's key column shows.
fn pending_label(param_key: &str, kind: &str) -> String {
    if kind != portal_storage::PENDING_KIND_THRESHOLD {
        return param_key.to_string();
    }
    if let Some((field, bound)) = crate::pending::parse_threshold_param_key(param_key) {
        let label = fiber_protocol::lorawan_field(&field)
            .map(|f| crate::i18n::field_label(crate::i18n::current_lang(), f.name, f.label))
            .unwrap_or(field);
        return format!("{label} · {bound}");
    }
    // `enabled` is drafted like a bound (OQ3), so it lands in this banner too.
    if let Some(field) = crate::pending::parse_threshold_enabled_key(param_key) {
        let label = fiber_protocol::lorawan_field(&field)
            .map(|f| crate::i18n::field_label(crate::i18n::current_lang(), f.name, f.label))
            .unwrap_or(field);
        return format!("{label} · enabled");
    }
    param_key.to_string()
}

/// A stored JSON scalar as the one line the table shows.
fn pending_value_text(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return "–".to_string();
    };
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Null) => "–".to_string(),
        Ok(serde_json::Value::String(s)) => s,
        Ok(other) => other.to_string(),
        // Not JSON at all: show the bytes rather than hiding them. Nothing
        // writes such a row today, and if something ever does the operator
        // should see what it wrote.
        Err(_) => raw.to_string(),
    }
}

/// The registry field a drafted threshold bound belongs to, `None` for
/// every other kind of draft row.
fn drafted_bound_field(
    r: &portal_storage::PendingConfigRow,
) -> Option<&'static fiber_protocol::LorawanFieldDef> {
    if r.kind != portal_storage::PENDING_KIND_THRESHOLD {
        return None;
    }
    let (field, _) = crate::pending::parse_threshold_param_key(&r.param_key)?;
    fiber_protocol::lorawan_field(&field)
}

/// A drafted bound (wire units) as displayed; anything that is not a number
/// reads exactly as [`pending_value_text`] would show it.
fn displayed_bound_text(raw: Option<&str>, f: &fiber_protocol::LorawanFieldDef) -> String {
    match raw.and_then(|r| serde_json::from_str::<f64>(r).ok()) {
        Some(v) => f.to_display(v).to_string(),
        None => pending_value_text(raw),
    }
}

/// How long past a walk's own budget a `sending` row is still believed. The budget is the
/// Node's own (its report interval, N13), so the banner does not call a live walk dead.
fn sending_stale_after_secs(budget: std::time::Duration) -> i64 {
    (budget + crate::pending::SENDING_STALE_SLACK).as_secs() as i64
}

/// Builds the banner from the stored draft plus the two "current value"
/// sources the detail page already has.
///
/// `thresholds` is the displayed view ([`displayed_node_thresholds`]), and a
/// drafted bound, stored in wire units, is shown in the same unit
/// (UX-038), so the two columns compare like with like.
///
/// `now` is a Unix timestamp, passed rather than read, because the one
/// judgement this function makes about time — whether a `sending` row has
/// been sending for impossibly long (§21 review, defect 2) — is otherwise
/// untestable without sleeping for four minutes.
pub fn pending_ctx(
    node_key: &str,
    rows: &[portal_storage::PendingConfigRow],
    config_entries: Option<&serde_json::Map<String, serde_json::Value>>,
    thresholds: &BTreeMap<String, ThresholdEntryCtx>,
    can_edit: bool,
    now: i64,
) -> PendingCtx {
    let budget =
        crate::pending::downlink_answer_budget(crate::pending::interval_report_of(config_entries));
    let rows: Vec<PendingRowCtx> = rows
        .iter()
        .map(|r| {
            // PF-3: a row the Hub already took quotes what the Node still holds, the value
            // from before the send, rather than the portal's (which is already the new one).
            let current = if let Some(previous) = r
                .previous_value
                .as_deref()
                .filter(|_| r.kind == portal_storage::PENDING_KIND_THRESHOLD)
            {
                Some(match drafted_bound_field(r) {
                    Some(f) => displayed_bound_text(Some(previous), f),
                    None => pending_value_text(Some(previous)),
                })
            } else if r.kind == portal_storage::PENDING_KIND_THRESHOLD {
                if let Some((field, bound)) =
                    crate::pending::parse_threshold_param_key(&r.param_key)
                {
                    thresholds.get(&field).and_then(|t| {
                        match bound {
                            "critical_low" => t.critical_low,
                            "warning_low" => t.warning_low,
                            "warning_high" => t.warning_high,
                            _ => t.critical_high,
                        }
                        .map(|v| v.to_string())
                    })
                } else if let Some(field) =
                    crate::pending::parse_threshold_enabled_key(&r.param_key)
                {
                    thresholds.get(&field).map(|t| t.enabled.to_string())
                } else {
                    None
                }
            } else {
                config_entries
                    .and_then(|e| e.get(&r.param_key))
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
            };
            // A `sending` row belongs to a walk in THIS process, and a walk
            // resolves every unit inside its own budget. One that has been
            // sending for longer than that belongs to a walk that no longer
            // exists — a task dropped so hard its own guard could not write
            // — and believing it leaves Send and Discard disabled forever
            // (§21 review, defect 2). The stored row is untouched; only the
            // banner's reading of it changes.
            let stale = r.state == portal_storage::PENDING_STATE_SENDING
                && now.saturating_sub(r.updated_at) > sending_stale_after_secs(budget);
            PendingRowCtx {
                label: pending_label(&r.param_key, &r.kind),
                param_key: r.param_key.clone(),
                kind: r.kind.clone(),
                desired: match drafted_bound_field(r) {
                    Some(f) => displayed_bound_text(r.desired_value.as_deref(), f),
                    None => pending_value_text(r.desired_value.as_deref()),
                },
                current,
                author: r.author.clone(),
                changed_at: format_time(r.updated_at),
                state: if stale {
                    portal_storage::PENDING_STATE_ERROR.to_string()
                } else {
                    r.state.clone()
                },
                last_error: r.last_error.clone(),
                hub_set: r.previous_value.is_some()
                    && r.kind == portal_storage::PENDING_KIND_THRESHOLD,
                stale,
            }
        })
        .collect();
    let undelivered = |r: &&PendingRowCtx| r.state == portal_storage::PENDING_STATE_UNDELIVERED;

    PendingCtx {
        node_key: node_key.to_string(),
        node_differs: rows
            .iter()
            .any(|r| r.author == crate::pending::NODE_DIFFERS_AUTHOR),
        alarm_layout_moved: rows
            .iter()
            .any(|r| r.author == crate::pending::ALARM_LAYOUT_AUTHOR),
        sending: rows
            .iter()
            .any(|r| r.state == portal_storage::PENDING_STATE_SENDING),
        failed: rows.iter().any(|r| {
            r.state == portal_storage::PENDING_STATE_ERROR
                || (r.state == portal_storage::PENDING_STATE_UNDELIVERED
                    && r.kind != portal_storage::PENDING_KIND_THRESHOLD)
        }),
        resend: rows.iter().any(|r| {
            r.state == portal_storage::PENDING_STATE_ERROR
                || r.state == portal_storage::PENDING_STATE_UNDELIVERED
        }),
        undelivered_bounds: rows
            .iter()
            .filter(undelivered)
            .any(|r| r.kind == portal_storage::PENDING_KIND_THRESHOLD),
        rows,
        can_edit,
        downlinks: None,
        confirm_minutes: budget.as_secs().div_ceil(60),
        refusal: None,
        awaiting: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::test_support::{
        bare, db_threshold_with_bounds, event, node, pending_row, raw, NOW,
    };

    /// devices.node.md §5, dodatek 2026-10-13: the Gateway column is the Node's home gateway.
    #[test]
    fn the_home_gateway_is_the_p2p_home_or_the_best_lorawan_gateway() {
        use crate::state::NodeGatewayLink;
        let homes = HomeGateways {
            server: Some("proximos-4e970f4a".into()),
            p2p: [
                ("5876070000000001".to_string(), None),
                (
                    "5876070000000002".to_string(),
                    Some("proximos-2200ad33".to_string()),
                ),
            ]
            .into_iter()
            .collect(),
            hubs_by_eui: [(
                "70b3d54900aa0002".to_string(),
                "proximos-2200ad33".to_string(),
            )]
            .into_iter()
            .collect(),
        };
        let p2p = |key: &str| {
            let mut n = NodeState::new(key.to_string(), NOW);
            n.transport = portal_domain::Transport::P2p;
            n
        };
        // `local` is the Server; a Client home is the Client; no session, no home.
        assert_eq!(
            homes.of(&p2p("5876070000000001")).as_deref(),
            Some("proximos-4e970f4a")
        );
        assert_eq!(
            homes.of(&p2p("5876070000000002")).as_deref(),
            Some("proximos-2200ad33")
        );
        assert_eq!(homes.of(&p2p("5876070000000003")), None);

        let link = |id: &str, rssi: Option<i64>| NodeGatewayLink {
            gateway_id: Some(id.to_string()),
            rssi,
            snr: None,
        };
        let mut lora = NodeState::new("70b3d5e050011234".into(), NOW);
        assert_eq!(homes.of(&lora), None, "never heard");
        lora.gateways = vec![
            link("70B3D54900AA0001", Some(-110)),
            link("70b3d54900aa0002", Some(-80)),
            link("70b3d54900aa0003", None),
        ];
        assert_eq!(
            homes.of(&lora).as_deref(),
            Some("proximos-2200ad33"),
            "the best gateway, named by its Hub"
        );
        lora.gateways = vec![
            link("70b3d54900aa0002", Some(-120)),
            link("70B3D54900AA0001", Some(-90)),
        ];
        assert_eq!(
            homes.of(&lora).as_deref(),
            Some("70b3d54900aa0001"),
            "an unknown gateway shows its EUI"
        );
    }

    #[test]
    fn normalize_radio_alarm_state_covers_all_firmware_states_and_default() {
        for (fw, want) in [
            ("NORMAL", "Normal"),
            ("WARNING", "Warning"),
            ("CRITICAL", "Critical"),
            ("DISCONNECTED", "Disconnected"),
            ("RECONNECTING", "Reconnecting"),
            ("NEVERCONNECTED", "NeverConnected"),
            ("NeverConnected", "NeverConnected"), // the model's own default spelling
        ] {
            assert_eq!(normalize_radio_alarm_state(fw), want, "state {fw}");
        }
        assert_eq!(
            normalize_radio_alarm_state("SOME_NEW_STATE"),
            "SOME_NEW_STATE"
        );
    }

    #[test]
    fn radio_connected_only_for_live_states() {
        for s in ["Normal", "Warning", "Critical"] {
            assert!(radio_connected(s), "{s}");
        }
        for s in ["Disconnected", "Reconnecting", "NeverConnected"] {
            assert!(!radio_connected(s), "{s}");
        }
    }

    #[test]
    fn node_card_ctx_battery_maps_from_voltage_field_not_the_fields_list() {
        let mut n = node("aabbccdd00000001");
        n.alarm_state = "NORMAL".to_string();
        n.fields.insert("voltage".to_string(), 3.6);
        n.fields.insert("temperature".to_string(), -18.0);
        n.field_alarm_states
            .insert("temperature".to_string(), "WARNING".to_string());

        let ctx = node_card_ctx(bare(&n));
        assert_eq!(ctx.battery, Some(3.6));
        assert_eq!(ctx.alarm_state, "Normal");
        assert!(ctx.connected);
        // voltage itself is not chartable — must not appear in the card's
        // per-field list, only feed `battery`.
        assert!(!ctx.fields.iter().any(|f| f.name == "voltage"));
        let temp = ctx
            .fields
            .iter()
            .find(|f| f.name == "temperature")
            .expect("temperature");
        assert_eq!(temp.value, -18.0);
        assert_eq!(temp.alarm_state, "WARNING");
    }

    /// UX-038: pressure reads in hPa on the Node row, in its threshold
    /// accordion (value and bounds alike) and on the detail page. The v2
    /// decoder sends hPa, shown as sent.
    #[test]
    fn a_nodes_pressure_is_shown_in_hpa() {
        let mut n = node("aabbccdd00000001");
        n.fields.insert("pressure".to_string(), 1013.288);
        n.field_thresholds = Some(vec![crate::state::NodeFieldThreshold {
            field: "pressure".into(),
            critical_low: Some(990.0),
            warning_low: Some(1000.0),
            warning_high: Some(1025.0),
            critical_high: Some(1035.0),
        }]);

        let ctx = node_card_ctx(bare(&n));
        let p = ctx.fields.iter().find(|f| f.name == "pressure").unwrap();
        assert!((p.value - 1013.288).abs() < 1e-9, "{}", p.value);
        assert_eq!(p.unit, "hPa");
        let band = ctx
            .threshold_fields
            .iter()
            .find(|f| f.name == "pressure")
            .unwrap();
        assert_eq!(band.unit, "hPa");
        assert!((band.value.unwrap() - 1013.288).abs() < 1e-9);
        let t = band.threshold.as_ref().unwrap();
        assert_eq!(
            [
                t.critical_low,
                t.warning_low,
                t.warning_high,
                t.critical_high
            ],
            [Some(990.0), Some(1000.0), Some(1025.0), Some(1035.0)]
        );

        let detail = node_detail_ctx(&n, None, None, None, Some(false), &Default::default());
        let p = detail.fields.iter().find(|f| f.name == "pressure").unwrap();
        assert_eq!(p.unit, "hPa");
        assert!((p.value.unwrap() - 1013.288).abs() < 1e-9);
    }

    /// STICKER 2.0.0: a quantity last sent as `null` keeps its row with no value ("–"); one
    /// never sent (sensor off or not reported) has no row without config info, as before.
    #[test]
    fn a_nulled_field_keeps_its_row_without_a_value() {
        let mut n = node("aabbccdd00000001");
        n.fields.insert("temperature".to_string(), 21.0);
        n.no_data_fields.insert("input_a_voltage".to_string());
        let detail = node_detail_ctx(&n, None, None, None, Some(false), &Default::default());
        let a = detail
            .fields
            .iter()
            .find(|f| f.name == "input_a_voltage")
            .expect("row stays");
        assert_eq!(a.value, None);
        assert!(!detail.fields.iter().any(|f| f.name == "input_b_voltage"));
        let card = node_card_ctx(bare(&n));
        assert!(!card.fields.iter().any(|f| f.name == "input_a_voltage"));
    }

    #[test]
    fn node_card_ctx_missing_field_alarm_state_defaults_to_normal() {
        let mut n = node("aabbccdd00000001");
        n.fields.insert("temperature".to_string(), 5.0);
        let ctx = node_card_ctx(bare(&n));
        let temp = ctx.fields.iter().find(|f| f.name == "temperature").unwrap();
        assert_eq!(temp.alarm_state, "NORMAL");
    }

    // ---- B1: the derived row data (devices.backend.md §4, gap A) --------

    /// devices.node.md §2.5's table, rung by rung, plus the two properties
    /// that table encodes but does not state: the top two rungs need BOTH
    /// floors, `weak` needs EITHER.
    /// The bands say "a band is set here", so they follow the band's own
    /// `enabled` switch and not whether a reading has arrived. Both halves
    /// were wrong before: a device registered a minute ago drew the muted bar
    /// although its thresholds were active, and a band switched off kept
    /// drawing its colours because the stored numbers were still there.
    #[test]
    fn a_threshold_gauge_follows_the_enabled_switch_not_the_reading() {
        let templates = crate::render::Templates::new("templates", "static");
        let env = templates.env();
        let gauge = |value: Option<f64>, enabled: bool| {
            env.render_str(
                r#"{% import "partials/ui.html" as ui %}{{ ui.threshold_gauge(value, t) }}"#,
                context! {
                    lang => "en",
                    value => value,
                    t => ThresholdEntryCtx {
                        critical_low: Some(-30.0),
                        warning_low: Some(-20.0),
                        warning_high: Some(30.0),
                        critical_high: Some(40.0),
                        enabled,
                        hysteresis: None,
                    },
                },
            )
            .expect("gauge renders")
        };

        let needle = "z-10";

        let measured = gauge(Some(23.8), true);
        assert!(measured.contains("bg-critical"), "bands: {measured}");
        assert!(measured.contains(needle), "needle at the reading");

        // Registered, thresholds active, nothing reported yet.
        let unmeasured = gauge(None, true);
        assert!(
            unmeasured.contains("bg-critical") && unmeasured.contains("bg-success"),
            "an active band draws without a reading: {unmeasured}"
        );
        assert!(!unmeasured.contains(needle), "and draws no needle");

        // Switched off: the stored numbers are not a band any more.
        let off = gauge(Some(23.8), false);
        assert!(!off.contains("bg-critical"), "no bands: {off}");
        assert!(off.contains("bg-muted"), "the muted bar instead");
    }

    /// P-4: one to three bounds draw a band too (they drew the empty bar, as if nothing were
    /// set), and a critical bound on its open side (P-3 presets) draws as no bound.
    #[test]
    fn a_threshold_gauge_draws_a_partial_band() {
        let templates = crate::render::Templates::new("templates", "static");
        let env = templates.env();
        let gauge = |t: ThresholdEntryCtx, field: Option<&str>| {
            env.render_str(
                r#"{% import "partials/ui.html" as ui %}{{ ui.threshold_gauge(21.0, t, field=field) }}"#,
                context! { lang => "en", t => t, field => field },
            )
            .expect("gauge renders")
        };
        let band = |cl, wl, wh, ch| ThresholdEntryCtx {
            critical_low: cl,
            warning_low: wl,
            warning_high: wh,
            critical_high: ch,
            enabled: true,
            hysteresis: None,
        };

        let warning_high = gauge(band(None, None, Some(25.0), None), None);
        assert!(
            warning_high.contains("bg-success") && warning_high.contains("z-10"),
            "one bound is a band with a needle: {warning_high}"
        );
        assert!(!warning_high.contains("bg-muted"), "{warning_high}");
        assert!(warning_high.contains("— / — / 25.0° / —"), "{warning_high}");

        let preset = gauge(
            band(Some(-99.0), Some(18.0), Some(25.0), Some(199.0)),
            Some("temperature"),
        );
        assert!(
            preset.contains("— / 18.0° / 25.0° / —"),
            "the open sides are not bounds: {preset}"
        );
        let critical_width = |html: &str| {
            let at = html.find(r#"class="bg-critical""#).expect("a red band");
            let style = &html[..at];
            let w = style.rfind("width:").expect("width");
            style[w + 6..]
                .split('%')
                .next()
                .unwrap()
                .parse::<f64>()
                .unwrap()
        };
        assert_eq!(critical_width(&preset), 0.0, "no red band at the low end");

        let full = gauge(band(Some(10.0), Some(18.0), Some(25.0), Some(30.0)), None);
        assert!(full.contains("10.0° / 18.0° / 25.0° / 30.0°"), "{full}");
        assert!(critical_width(&full) > 0.0);

        let none = gauge(band(None, None, None, None), None);
        assert!(none.contains("bg-muted"), "no bound, the empty bar: {none}");
    }

    /// P-10: every key the raw message offers has the Portal's own help in both languages, so
    /// a key a firmware update adds shows up here rather than as CLI prose in the UI.
    #[test]
    fn every_raw_message_key_has_its_own_help() {
        for tpl in raw_message_templates(None) {
            for k in tpl.keys {
                let key = format!("monitoring.node_param_help.{}", k.key);
                for lang in ["cs", "en"] {
                    assert_ne!(crate::i18n::lookup(lang, &key), key, "{lang}: {key}");
                }
            }
        }
    }

    #[test]
    fn signal_level_walks_the_specified_ladder() {
        let b = crate::config::SignalQualityConfig::default();
        let at = |rssi, snr| signal_level(Some(rssi), Some(snr), &b);

        assert_eq!(at(-70, 9.0), SignalLevel::Excellent);
        assert_eq!(at(-80, 7.0), SignalLevel::Excellent, "floors are inclusive");

        // Clears excellent's RSSI but not its SNR — falls to good, not
        // excellent. This is the `and` in the top rungs.
        assert_eq!(at(-70, 3.0), SignalLevel::Good);
        assert_eq!(at(-95, 0.0), SignalLevel::Good);

        // Fails good on both, but clears weak on ONE. This is the `or`.
        assert_eq!(at(-100, -20.0), SignalLevel::Weak, "rssi alone carries it");
        assert_eq!(at(-130, -3.0), SignalLevel::Weak, "snr alone carries it");

        assert_eq!(at(-130, -20.0), SignalLevel::Critical);
    }

    /// Missing data is `Unknown`, which is not a rung: reading an absent SNR
    /// as "fails the floor" would report a healthy link as weak.
    #[test]
    fn signal_level_is_unknown_when_a_measurement_is_missing() {
        let b = crate::config::SignalQualityConfig::default();
        assert_eq!(signal_level(None, Some(9.0), &b), SignalLevel::Unknown);
        assert_eq!(signal_level(Some(-70), None, &b), SignalLevel::Unknown);
        assert_eq!(signal_level(None, None, &b), SignalLevel::Unknown);

        // And `Unknown` sorts after `Critical` — an unmeasured link must not
        // bury the genuinely bad ones at the top of a signal-sorted list.
        assert!(SignalLevel::Unknown.severity_rank() > SignalLevel::Critical.severity_rank());
    }

    /// The bands come from the file, so a different file gives a different
    /// verdict for the same measurements (devices.node.md §2.5's whole point).
    #[test]
    fn signal_level_follows_the_configured_bands_not_hardcoded_numbers() {
        use crate::config::{SignalBand, SignalQualityConfig};
        let strict = SignalQualityConfig {
            excellent: SignalBand {
                rssi: -50,
                snr: 12.0,
            },
            good: SignalBand {
                rssi: -60,
                snr: 10.0,
            },
            weak: SignalBand {
                rssi: -70,
                snr: 8.0,
            },
        };
        // Excellent under the shipped defaults, critical under these.
        assert_eq!(
            signal_level(Some(-75), Some(7.5), &SignalQualityConfig::default()),
            SignalLevel::Excellent
        );
        assert_eq!(
            signal_level(Some(-75), Some(7.5), &strict),
            SignalLevel::Critical
        );
    }

    /// The operational axis is independent of the value axis: a Node
    /// reporting a perfectly normal temperature over a dead link is a fault,
    /// and `alarm_state` must not absorb that.
    #[test]
    fn offline_is_a_fault_without_touching_the_value_axis() {
        let mut n = node("aabbccdd00000001");
        n.alarm_state = "NORMAL".to_string();
        n.fields.insert("temperature".to_string(), 4.0);

        let healthy = node_card_ctx(bare(&n));
        assert_eq!(healthy.fault, None);
        assert_eq!(healthy.alarm_state, "Normal");

        let ctx = node_card_ctx(NodeCardInputs {
            offline: Some(true),
            ..bare(&n)
        });
        assert_eq!(ctx.fault, Some(NodeFault::Offline));
        assert_eq!(ctx.alarm_state, "Normal", "the value axis is untouched");
    }

    /// GitLab #80, at the row rather than at the macro: a band the operator
    /// switched off must read as "no bounds set" in the Meze column, so the
    /// list answers "are there thresholds here" at a glance. Rendered through
    /// `pages/nodes.html`, because the macro honouring `enabled` and the row
    /// actually handing it the band are two different things to get right.
    #[test]
    fn a_switched_off_band_draws_no_bands_in_the_node_row() {
        let mut n = node("aabbccdd00000001");
        n.alarm_state = "NORMAL".to_string();
        n.fields.insert("temperature".to_string(), 4.0);

        let render = |enabled: bool| {
            let rows = vec![db_threshold_with_bounds("temperature", enabled)];
            let ctx = node_card_ctx(NodeCardInputs {
                thresholds: &rows,
                ..bare(&n)
            });
            let templates = Templates::new("templates", "static");
            let html = templates
                .env()
                .get_template("pages/nodes.html")
                .expect("template loads")
                .render(context! {
                    lang => "en",
                    current_user => None::<CurrentUser>,
                    hostname => "h1",
                    nodes => vec![ctx],
                    can_edit => false,
                })
                .expect("nodes page renders");
            // Inside the ROW: the head carries the same `data-column`, and
            // it comes first in the document.
            let row_at = html.find(r#"<tr id="node-"#).expect("the row");
            let row = &html[row_at..][..html[row_at..].find("</tr>").expect("row ends")];
            let cell_at = row.find(r#"data-column="bounds""#).expect("the Meze cell");
            row[cell_at..][..row[cell_at..].find("</td>").expect("cell ends")].to_string()
        };

        let on = render(true);
        assert!(on.contains("bg-critical"), "the bands are drawn: {on}");

        let off = render(false);
        assert!(!off.contains("bg-critical"), "no bands: {off}");
        assert!(off.contains("bg-muted"), "the empty bar instead: {off}");
    }

    /// The `/nodes` toolbar (devices.node.md §2.2-§2.4) is client-side over
    /// the row's `data-*`, so template and script have a contract no
    /// compiler checks. This pins both ends of it.
    ///
    /// The cell count is the half that fails silently: the dynamic columns
    /// are rendered always and hidden by CSS, so a head and a body that
    /// disagree do not throw — they shear the table sideways, and only on
    /// the rows that happen to be on screen.
    #[test]
    fn the_nodes_page_and_its_rows_agree_on_the_toolbar_contract() {
        let mut n = node("aabbccdd00000001");
        n.alarm_state = "NORMAL".to_string();
        n.fields.insert("temperature".to_string(), 4.0);
        n.fields.insert("voltage".to_string(), 3.641);
        n.location = Some("Sklad B".to_string());
        let ctx = node_card_ctx(bare(&n));

        let templates = Templates::new("templates", "static");
        let html = templates
            .env()
            .get_template("pages/nodes.html")
            .expect("template loads")
            .render(context! {
                lang => "en",
                current_user => None::<CurrentUser>,
                hostname => "h1",
                nodes => vec![ctx],
                can_edit => false,
                profiles => Vec::<String>::new(),
            })
            .expect("nodes page renders");

        // The DOM contract portal-list.js looks for.
        assert!(html.contains(r#"data-list-key="nodes""#), "the wrapper");
        assert!(html.contains("data-list-search"), "the fulltext box");
        for filter in ["location", "connection", "oor", "proto"] {
            assert!(
                html.contains(&format!(r#"data-list-filter="{filter}""#)),
                "the {filter} select"
            );
        }
        assert!(html.contains("data-list-gwselect"), "the gateway select");
        assert!(html.contains("data-list-nomatch"), "the empty-filter row");
        // The enerooo-shaped disclosure: the panel and its always-visible
        // header are separate elements, and the script needs all three.
        assert!(
            html.contains("data-list-filters-toggle"),
            "the Filters button"
        );
        assert!(html.contains("data-list-panel"), "the filter panel");
        assert!(html.contains("data-list-grid"), "the collapsing field grid");
        assert!(html.contains("data-list-count"), "the result count");
        // Every informational column is toggleable and addressed by
        // `data-column` on BOTH ends — the base four, the diagnostic tail,
        // and Protocol, which used to be the one exception: it drew itself
        // on a mixed fleet through a class of its own and the picker had no
        // entry for it, so the only column an operator could not switch off
        // was the one that turned up uninvited.
        for col in [
            "loc", "overview", "values", "bounds", "hub", "seen", "rssi", "snr", "batt", "fw",
            "proto",
        ] {
            assert!(
                html.contains(&format!(r#"data-col="{col}""#)),
                "the {col} checkbox"
            );
            assert!(
                html.matches(&format!(r#"data-column="{col}""#)).count() >= 2,
                "the {col} head cell and its body cell"
            );
        }
        // Strong defaults: the row as §2.3 describes it is on, the tail is
        // off. A picker where everything defaults off opens a blank table.
        assert_eq!(
            html.matches(r#"data-col-default="1""#).count(),
            5,
            "Location, Overview, Values, Limits and Hub default to visible"
        );
        // Name and Detail are the row's identity and its control, so neither
        // is on offer — hiding the sticky Name column would leave an
        // unidentifiable row scrolling sideways.
        assert!(
            !html.contains(r#"data-col="name""#) && !html.contains(r#"data-col="detail""#),
            "identity and controls are not columns to hide"
        );
        // A way back: the picker is per-browser and silent, so without this
        // a viewer who hid the wrong thing has to remember what was on.
        assert!(
            html.contains("data-list-cols-reset"),
            "the picker offers a reset to the declared defaults"
        );
        // This page is rendered for a viewer, so the primary action is not
        // there at all — the caller gates it by not passing a label. The
        // routes gate independently; this keeps a viewer from clicking into
        // a refusal.
        assert!(
            !html.contains("data-list-action"),
            "a viewer is not offered the register button"
        );

        // The row's own half of it.
        let row_start = html.find(r#"id="node-h1-"#).expect("the row is rendered");
        let row = &html[row_start..][..html[row_start..].find("</tr>").expect("row ends")];
        assert!(row.contains(r#"data-oor="normal""#), "the value class");
        assert!(row.contains(r#"data-connected="#), "the operational class");
        assert!(
            row.contains(r#"data-location="Sklad B""#),
            "the location key"
        );
        assert!(row.contains(r#"data-sort-name="#), "a sort key");
        assert!(row.contains("3.64 V"), "battery is x.yy V (§2.6)");

        // Head and body, same width.
        let head_start = html.find("<thead").expect("a head");
        let head = &html[head_start..][..html[head_start..].find("</thead>").expect("head ends")];
        assert_eq!(
            head.matches("<th ").count(),
            row.matches("<td").count(),
            "the head and the row must declare the same number of columns"
        );
    }

    /// "A null measurement is a fault, not out-of-range" — narrowed to the
    /// case the portal can actually identify: the Node reported an alarm
    /// state for a field it sent no value for.
    #[test]
    fn a_field_with_an_alarm_state_but_no_value_is_a_fault() {
        let mut n = node("aabbccdd00000001");
        n.alarm_state = "NORMAL".to_string();
        n.field_alarm_states
            .insert("temperature".to_string(), "WARNING".to_string());

        let ctx = node_card_ctx(bare(&n));
        assert_eq!(ctx.fault, Some(NodeFault::MissingMeasurement));

        // With the value present it is an ordinary out-of-range, not a fault.
        n.fields.insert("temperature".to_string(), 40.0);
        assert_eq!(node_card_ctx(bare(&n)).fault, None);
    }

    // ---- §19 (F2 + F3): one liveness verdict ----------------------------

    /// F2. The Hub's last echo said `NORMAL`; the detector has measured the
    /// Node as silent past its budget. Before §19 the row rendered the fault
    /// triangle beside a *green* dot, because `connected` came off the echo
    /// and `fault` off the detector with nothing reconciling them.
    #[test]
    fn the_detector_overrides_a_stale_hub_echo() {
        let mut n = node("aabbccdd00000001");
        n.alarm_state = "NORMAL".to_string();

        let ctx = node_card_ctx(NodeCardInputs {
            offline: Some(true),
            ..bare(&n)
        });
        assert!(!ctx.connected, "the detector, not the echo, decides");
        assert_eq!(ctx.fault, Some(NodeFault::Offline));
        assert_eq!(
            ctx.alarm_state, "Normal",
            "the value axis is untouched — §3.1's two indicators stay two"
        );
    }

    /// F3. A Node fed only by the per-Node v2 envelope is never written into
    /// `NodeState::alarm_state`, so it stays `NeverConnected` for its whole
    /// life however recently it reported. The detector has heard from it, so
    /// the detector answers.
    #[test]
    fn a_v2_envelope_node_is_connected_once_the_detector_has_measured_it() {
        let mut n = node("aabbccdd00000001");
        n.alarm_state = "NeverConnected".to_string();

        let ctx = node_card_ctx(NodeCardInputs {
            offline: Some(false),
            ..bare(&n)
        });
        assert!(ctx.connected);
        assert_eq!(ctx.fault, None);
    }

    /// The echo is the fallback, and only the fallback: with no verdict —
    /// before the first sweep, detection disabled, or a Node never heard
    /// from — the row reads exactly as it did before §19. `NeverConnected`
    /// staying disconnected is the case that matters: a registered Node
    /// waiting to join must not come out of this looking alive.
    #[test]
    fn without_a_verdict_the_hub_echo_still_decides() {
        assert!(node_connected("Normal", None));
        assert!(!node_connected("NeverConnected", None));
        assert!(!node_connected("Disconnected", None));

        let mut n = node("aabbccdd00000001");
        n.alarm_state = "NeverConnected".to_string();
        let ctx = node_card_ctx(NodeCardInputs {
            offline: None,
            ..bare(&n)
        });
        assert!(!ctx.connected);
        assert_eq!(ctx.fault, None, "no verdict is not a fault either");
    }

    // ---- §19 (F12): the history fault -----------------------------------

    /// A Node whose latest backfill job ran out of retries. Before §19 this
    /// raised `SystemAlertKind::NodeHistoryUnavailable` into the notifier and
    /// nothing else, so with no recipients configured it reached nobody.
    #[test]
    fn a_terminal_backfill_job_is_a_fault_ranked_under_offline() {
        let mut n = node("aabbccdd00000001");
        n.alarm_state = "NORMAL".to_string();
        // The flag `backfill.rs` syncs from the jobs table (#64).
        n.history_unavailable = true;

        let ctx = node_card_ctx(bare(&n));
        assert_eq!(ctx.fault, Some(NodeFault::HistoryUnavailable));
        assert!(ctx.connected, "a history hole is not a liveness verdict");

        // Silence explains the unanswered downlinks, so it outranks them.
        let ctx = node_card_ctx(NodeCardInputs {
            offline: Some(true),
            ..bare(&n)
        });
        assert_eq!(ctx.fault, Some(NodeFault::Offline));
    }

    /// It also outranks a missing measurement, which is the narrowest of the
    /// three signals.
    #[test]
    fn the_history_fault_outranks_a_missing_measurement() {
        let mut n = node("aabbccdd00000001");
        n.field_alarm_states
            .insert("temperature".to_string(), "WARNING".to_string());
        n.history_unavailable = true;
        let ctx = node_card_ctx(bare(&n));
        assert_eq!(ctx.fault, Some(NodeFault::HistoryUnavailable));
    }

    /// The detail page reads the same fault terms as the row, so the two
    /// cannot disagree — and `fault_since` is only ever set for the one kind
    /// that has an honest answer.
    #[test]
    fn the_detail_page_agrees_with_the_row_and_dates_only_the_offline_fault() {
        let mut n = node("aabbccdd00000001");
        n.alarm_state = "NORMAL".to_string();
        n.last_seen = Some("2026-09-07T08:00:00Z".to_string());

        let offline = node_detail_ctx(&n, None, None, None, Some(true), &Default::default());
        assert!(!offline.connected);
        assert_eq!(offline.fault, Some(NodeFault::Offline));
        assert_eq!(offline.fault_since.as_deref(), Some("2026-09-07T08:00:00Z"));

        n.history_unavailable = true;
        let history = node_detail_ctx(&n, None, None, None, Some(false), &Default::default());
        assert!(history.connected);
        assert_eq!(history.fault, Some(NodeFault::HistoryUnavailable));
        assert_eq!(
            history.fault_since, None,
            "only silence has a 'since when' the portal actually knows"
        );

        n.history_unavailable = false;
        let healthy = node_detail_ctx(&n, None, None, None, Some(false), &Default::default());
        assert_eq!(healthy.fault, None);
    }

    /// Offline outranks a missing measurement: a Node nobody can hear is the
    /// fault that explains the other symptom.
    #[test]
    fn offline_outranks_a_missing_measurement() {
        let mut n = node("aabbccdd00000001");
        n.field_alarm_states
            .insert("temperature".to_string(), "WARNING".to_string());
        let ctx = node_card_ctx(NodeCardInputs {
            offline: Some(true),
            ..bare(&n)
        });
        assert_eq!(ctx.fault, Some(NodeFault::Offline));
    }

    #[test]
    fn the_row_carries_metadata_gateways_and_firmware() {
        let mut n = node("aabbccdd00000001");
        n.gateways = vec![NodeGatewayLink {
            gateway_id: Some("gw-1".into()),
            rssi: Some(-70),
            snr: Some(9.0),
        }];
        let meta = portal_storage::DeviceMeta {
            hostname: "h1".into(),
            scope: portal_storage::THRESHOLD_SCOPE_NODE.into(),
            entity: "aabbccdd00000001".into(),
            name: None,
            description: Some("cold room".into()),
            created_at: "2026-01-13 14:30:00".into(),
            added_by: Some("alice".into()),
            offline_k_override: None,
            backfill_auto: true,
            store_interval_override: None,
            serial: None,
            location: None,
        };
        let info = serde_json::json!({ "fw_version": "v1.4.0" });

        let ctx = node_card_ctx(NodeCardInputs {
            info: Some(&info),
            meta: Some(&meta),
            ..bare(&n)
        });

        assert_eq!(ctx.description.as_deref(), Some("cold room"));
        assert_eq!(ctx.created_at.as_deref(), Some("2026-01-13 14:30:00"));
        assert_eq!(ctx.fw_version.as_deref(), Some("v1.4.0"));
        assert_eq!(ctx.gateways.len(), 1);
        assert_eq!(ctx.gateways[0].gateway_id.as_deref(), Some("gw-1"));
    }

    /// The firmware version is dug out of a payload passed through verbatim
    /// from the wire, so both spellings in use are accepted — and a miss is
    /// `None`, never a guess.
    #[test]
    fn firmware_version_accepts_both_wire_spellings() {
        assert_eq!(
            fw_version_from_info(&serde_json::json!({"fw_version": "v1.4.0"})).as_deref(),
            Some("v1.4.0")
        );
        assert_eq!(
            fw_version_from_info(&serde_json::json!({"info": {"fw_version": "v1.4.0"}})).as_deref(),
            Some("v1.4.0")
        );
        assert_eq!(
            fw_version_from_info(&serde_json::json!({"serial_number": "SN-1"})),
            None
        );
    }

    /// Per-field threshold state rides the row so the High/Low x
    /// Warning/Critical filter can evaluate client-side. Reuses
    /// `merge_node_thresholds`, so the row and the detail page cannot
    /// disagree about what a threshold is.
    #[test]
    fn the_row_carries_merged_per_field_threshold_state() {
        let n = node("aabbccdd00000001");
        let stored = vec![FieldThreshold {
            hostname: "h1".into(),
            scope: portal_storage::THRESHOLD_SCOPE_NODE.into(),
            entity: "aabbccdd00000001".into(),
            field: "temperature".into(),
            critical_low: Some(-30.0),
            warning_low: Some(-25.0),
            warning_high: Some(-5.0),
            critical_high: Some(0.0),
            enabled: true,
            updated_at: None,
            hysteresis: None,
        }];

        let ctx = node_card_ctx(NodeCardInputs {
            thresholds: &stored,
            ..bare(&n)
        });

        let t = ctx
            .thresholds
            .get("temperature")
            .expect("temperature entry");
        assert_eq!(t.critical_low, Some(-30.0));
        assert!(t.enabled);
        // A field with no threshold anywhere is absent, not defaulted — the
        // `Not set` filter reads absence.
        assert!(!ctx.thresholds.contains_key("humidity"));
    }

    #[test]
    fn a_ring_message_decodes_its_values_and_signal() {
        let ctx = node_message_ctx(&raw(
            1,
            1_768_314_600,
            r#"{"type":"reading","fields":{"temperature":-18.5},"rssi":-72,"snr":8.25}"#,
        ));

        assert_eq!(ctx.kind, "reading");
        assert_eq!(ctx.rssi, Some(-72));
        assert_eq!(ctx.snr, Some(8.25));
        assert_eq!(ctx.values["temperature"], minijinja::Value::from(-18.5));
        assert_eq!(ctx.received_at.as_deref(), Some("2026-01-13T14:30:00Z"));
    }

    /// A payload that will not parse still produces a row. Dropping it would
    /// make the ring silently shorter than the ten messages the operator was
    /// promised, and "something unreadable arrived at 14:30" is exactly the
    /// kind of thing they opened this page to find.
    #[test]
    fn an_unparseable_ring_message_still_renders() {
        let ctx = node_message_ctx(&raw(1, 1_768_314_600, "not json at all"));
        assert_eq!(ctx.kind, "unparsed");
        assert!(ctx.values.is_empty());
        assert_eq!(ctx.rssi, None);
        assert_eq!(ctx.received_at.as_deref(), Some("2026-01-13T14:30:00Z"));
    }

    /// A well-formed payload that simply does not name its own type is a
    /// `message`, not an `unparsed` — the distinction is whether the bytes
    /// were readable.
    #[test]
    fn a_typeless_payload_is_a_message_not_an_unparsed_one() {
        let ctx = node_message_ctx(&raw(1, 0, r#"{"fields":{"humidity":41.0}}"#));
        assert_eq!(ctx.kind, "message");
        assert_eq!(ctx.values["humidity"], minijinja::Value::from(41.0));
    }

    /// Rows arrive newest-first, so the first alarm-ish row wins — and the
    /// ordinary telemetry rows in between are skipped rather than reported
    /// as the last alarm.
    #[test]
    fn the_last_alarm_is_the_newest_alarm_not_the_newest_event() {
        let events = [
            event(300, "reading", None),
            event(
                200,
                "threshold_alarm",
                Some(r#"{"field":"temperature","value":4.5,"threshold":4.0}"#),
            ),
            event(
                100,
                "threshold_alarm",
                Some(r#"{"field":"temperature","value":9.0}"#),
            ),
        ];

        let last = last_alarm_ctx(&events).expect("an alarm event");
        assert_eq!(last.event_type, "threshold_alarm");
        assert_eq!(last.field.as_deref(), Some("temperature"));
        assert_eq!(last.value, Some(4.5));
        assert_eq!(last.threshold, Some(4.0));
    }

    /// Matched on substring, not an allowlist: `node_events` is free-text
    /// typed and shared by every decoded event, so an exact-string list
    /// would go stale the first time the firmware adds a kind.
    #[test]
    fn fault_events_count_as_alarms_and_plain_ones_do_not() {
        assert!(last_alarm_ctx(&[event(1, "sensor_fault", None)]).is_some());
        assert!(last_alarm_ctx(&[event(1, "ALARM_CLEARED", None)]).is_some());
        assert!(last_alarm_ctx(&[event(1, "reading", None)]).is_none());
        assert!(last_alarm_ctx(&[]).is_none());
    }

    /// A v2 fPort-3 page lands as an `alarm_batch` header plus its alarms at the same
    /// instant. The card is the alarm, not the header, in both the current wire shape and
    /// the one a Hub built before the Nodes test 2026-09-23 publishes (`type` = the side).
    #[test]
    fn the_last_alarm_of_a_v2_batch_is_the_alarm_not_its_header() {
        let header = event(
            1_790_204_831,
            "alarm_batch",
            Some(r#"{"type":"alarm_batch","total":1,"time_synced":true}"#),
        );
        for alarm in [
            event(
                1_790_204_831,
                "alarm",
                Some(
                    r#"{"type":"alarm","event":"activate","side":"high","quantity":"temperature","slot":0,"value":27.79}"#,
                ),
            ),
            event(
                1_790_204_831,
                "high",
                Some(
                    r#"{"type":"high","event":"activate","quantity":"temperature","slot":0,"value":27.79}"#,
                ),
            ),
        ] {
            let last = last_alarm_ctx(&[header.clone(), alarm]).expect("the alarm is found");
            assert_eq!(last.event_type, "alarm · high · activate");
            assert_eq!(
                (last.side.as_deref(), last.edge.as_deref()),
                (Some("high"), Some("activate"))
            );
            let templates = crate::render::Templates::new("templates", "static");
            let badge = templates
                .env()
                .render_str(
                    r#"{% import "partials/ui.html" as ui %}{{ ui.alarm_event_badge(a) }}"#,
                    context! { lang => "cs", a => &last },
                )
                .expect("badge renders");
            assert!(badge.contains("Horní mez · vznik"), "{badge}");
            assert_eq!(last.field.as_deref(), Some("temperature"));
            assert_eq!(last.value, Some(27.79));
        }
        assert!(
            last_alarm_ctx(&[header]).is_none(),
            "a header alone is no alarm"
        );
    }

    /// The fPort-3 ring row has no `fields`; it shows its alarm events instead of nothing.
    #[test]
    fn an_alarm_batch_row_in_the_ring_shows_its_alarms() {
        let ctx = node_message_ctx(&raw(
            1,
            1_790_204_831,
            r#"{"fport":3,"fields":{},"events":[
                {"type":"alarm_batch","total":2},
                {"type":"alarm","event":"activate","side":"high","quantity":"temperature","slot":0,"value":27.79},
                {"type":"alarm","event":"deactivate","side":"low","quantity":"temperature","slot":1,"value":19.5}]}"#,
        ));
        assert_eq!(ctx.port, Some(3));
        assert_eq!(
            ctx.values
                .get("temperature")
                .map(ToString::to_string)
                .as_deref(),
            Some("activate high 27.79")
        );
        assert_eq!(
            ctx.values
                .get("temperature (slot 1)")
                .map(ToString::to_string)
                .as_deref(),
            Some("deactivate low 19.5")
        );
        assert_eq!(
            ctx.values.len(),
            2,
            "the header is no value: {:?}",
            ctx.values
        );
    }

    /// F8: the card shows volts and flag names, not the Node's raw mV and bitmasks, in the
    /// v2.0.0 layout.
    #[test]
    fn the_device_card_decodes_battery_status_and_reset_cause() {
        let v15 = DeviceInfoView::of(
            &serde_json::json!({
                "fw_major": 1, "fw_minor": 5, "fw_patch": 0,
                "battery": 3280, "device_status": (1 << 0) | (1 << 1) | (1 << 9) | (1 << 17) | (1 << 20),
                "reset_cause": 3,
            }),
            None,
        );
        assert_eq!(v15.battery.as_deref(), Some("3.28 V"));
        assert_eq!(
            v15.status_flags,
            Some(vec![
                "alarm_any".to_string(),
                "alarm_threshold".to_string(),
                "lrw_disabled".to_string(),
                "claim_active".to_string(),
                "bit20".to_string()
            ]),
            "a bit v2.0.0 does not define shows as bitN"
        );
        assert_eq!(
            v15.reset_causes,
            Some(vec!["pin".to_string(), "software".to_string()])
        );
        assert_eq!(
            DeviceInfoView::of(&serde_json::json!({"device_status": 1 << 15}), None).status_flags,
            Some(vec!["history_down".to_string()])
        );

        let unavailable = DeviceInfoView::of(
            &serde_json::json!({"battery": null, "device_status": 0, "reset_cause": 0}),
            None,
        );
        assert_eq!(unavailable.battery.as_deref(), Some("–"));
        assert_eq!(unavailable.status_flags, Some(vec![]), "empty is all OK");
        assert_eq!(unavailable.reset_causes, Some(vec![]), "empty is unknown");

        for (secs, text) in [
            (18, "18 s"),
            (125, "2 min 5 s"),
            (6645, "1 h 50 min"),
            (277_200, "3 d 5 h"),
        ] {
            assert_eq!(
                DeviceInfoView::of(&serde_json::json!({"uptime_s": secs}), None)
                    .uptime
                    .as_deref(),
                Some(text)
            );
        }

        let quiet = DeviceInfoView::of(&serde_json::json!({"fw_major": 1, "fw_minor": 5}), None);
        assert_eq!(
            quiet.status_flags,
            Some(vec![]),
            "proto3: an absent mask is 0"
        );
        assert_eq!(quiet.reset_causes, Some(vec![]));

        let v1 = DeviceInfoView::of(&serde_json::json!({"fw_version": "v1.3.9"}), None);
        assert!(v1.battery.is_none() && v1.status_flags.is_none() && v1.reset_causes.is_none());
        assert!(v1.clock.is_none() && v1.active_alarms.is_none());
    }

    /// WP7: the Node's clock against the Hub's receive time, and its active alarms by name.
    #[test]
    fn the_device_card_reads_the_clock_and_the_active_alarms() {
        let synced = DeviceInfoView::of(
            &serde_json::json!({
                "unix_time": 1_790_244_003_u64, "device_status": 1,
                "active_alarms": [
                    {"slot": 0, "channel": 0, "type": 2},
                    {"slot": 2, "channel": 0, "type": 1, "sensor_type": 2},
                    {"source": 11, "quantity": 8, "type": 1},
                ],
            }),
            Some("2026-09-24T10:00:00Z"),
        );
        let clock = synced.clock.expect("clock");
        assert!(clock.synced);
        assert_eq!(clock.offset_s, Some(3), "Node minus Hub");
        assert_eq!(clock.node_time.as_deref(), Some("2026-09-24T10:00:03Z"));
        let alarms = synced.active_alarms.expect("alarms");
        let named: Vec<(String, Option<u64>, String, String)> = alarms
            .into_iter()
            .map(|a| (a.source, a.slot, a.quantity, a.kind))
            .collect();
        let owned =
            |s: &str, slot, q: &str, k: &str| (s.to_string(), slot, q.to_string(), k.to_string());
        // A pre-2.0.0 `{source, quantity}` entry names nothing but its type.
        assert_eq!(
            named,
            [
                owned("onboard", None, "temperature", "high"),
                owned("slot", Some(2), "temperature", "low"),
                owned("", None, "", "low"),
            ]
        );

        // No wall clock yet: counting from 0, or the firmware says so.
        for doc in [
            serde_json::json!({"unix_time": 5_000, "device_status": 0}),
            serde_json::json!({"unix_time": 1_790_244_003_u64, "device_status": 1 << 16}),
        ] {
            let clock = DeviceInfoView::of(&doc, Some("2026-09-24T10:00:00Z"))
                .clock
                .expect("clock");
            assert!(!clock.synced, "{doc}");
            assert!(clock.offset_s.is_none() && clock.node_time.is_none());
        }
        // v2.0.0: slot, channel and the slot's type.
        let v200 = DeviceInfoView::of(
            &serde_json::json!({"active_alarms": [
                {"slot": 0, "channel": 2, "type": 2},
                {"slot": 1, "channel": 2, "type": 1, "sensor_type": 3},
                {"slot": 2, "channel": 0, "type": 5, "sensor_type": 2,
                 "type_name": "sensor_mismatch", "sensor_type_name": "dallas"},
            ]}),
            None,
        );
        let named: Vec<(String, Option<u64>, String, String)> = v200
            .active_alarms
            .expect("alarms")
            .into_iter()
            .map(|a| (a.source, a.slot, a.quantity, a.kind))
            .collect();
        assert_eq!(
            named,
            [
                owned("onboard", None, "pressure", "high"),
                owned("slot", Some(1), "temperature", "low"),
                owned("slot", Some(2), "dallas", "sensor_mismatch"),
            ]
        );
        let slots = DeviceInfoView::of(
            &serde_json::json!({"w1_slot_state": ["ok", "absent", "none", "mismatch"]}),
            None,
        );
        assert_eq!(
            slots.w1_slot_state.as_deref(),
            Some(&["ok", "absent", "none", "mismatch"].map(String::from)[..])
        );
        let none = DeviceInfoView::of(&serde_json::json!({"active_alarms": []}), None);
        assert_eq!(
            none.active_alarms.map(|a| a.len()),
            Some(0),
            "empty is none"
        );
    }

    /// WP3: the Diagnostics card lists the sensors the config info declares.
    #[test]
    fn the_diagnostics_card_lists_the_active_sensors() {
        let n = node("aabbccdd00000001");
        let cfg = config_entry(
            &[
                ("sensors.cap_barometer", serde_json::Value::Bool(true)),
                ("sensors.cap_w1_sensors", serde_json::Value::Bool(true)),
            ],
            Some(&["dallas", "empty", "empty", "empty"]),
        );
        let templates = Templates::new("templates", "static");
        let render = |ctx: NodeDetailCtx| {
            templates
                .env()
                .get_template("pages/node_detail.html")
                .expect("template loads")
                .render(context! {
                    lang => "en",
                    current_user => None::<CurrentUser>,
                    hostname => "h1",
                    n => ctx,
                    active_tab => "diagnostics",
                    thresholds => BTreeMap::<String, ThresholdEntryCtx>::new(),
                    config => context! {
                        entries => None::<serde_json::Value>,
                        read_status => None::<String>,
                        synced_at => None::<String>,
                        missing => Vec::<String>::new(),
                    },
                    jobs => Vec::<crate::backfill::JobCtx>::new(),
                    gaps => Vec::<crate::backfill::GapCtx>::new(),
                    can_edit => false,
                })
                .expect("detail page renders")
        };
        let html = render(node_detail_ctx(
            &n,
            None,
            None,
            Some(&cfg),
            Some(false),
            &Default::default(),
        ));
        assert!(html.contains("Active channels"));
        assert!(html.contains("Barometer MPL3115A2"));
        assert!(
            html.contains("1-Wire slot 1 &middot; Dallas (DS18B20)"),
            "{html}"
        );

        let none = render(node_detail_ctx(
            &n,
            None,
            None,
            None,
            Some(false),
            &Default::default(),
        ));
        assert!(none.contains("has not sent its config info yet"));
    }

    /// F8 on the page: volts and named flags on the Diagnostics card, not `3458` and `3`.
    #[test]
    fn the_diagnostics_card_renders_the_decoded_device_info() {
        let n = node("aabbccdd00000001");
        let mut di = v2_device_info();
        di.info["device_status"] = serde_json::json!(0);
        let templates = Templates::new("templates", "static");
        let html = templates
            .env()
            .get_template("pages/node_detail.html")
            .expect("template loads")
            .render(context! {
                lang => "en",
                current_user => None::<CurrentUser>,
                hostname => "h1",
                n => node_detail_ctx(&n, None, Some(&di), None, Some(false), &Default::default()),
                active_tab => "diagnostics",
                thresholds => BTreeMap::<String, ThresholdEntryCtx>::new(),
                config => context! {
                    entries => None::<serde_json::Value>,
                    read_status => None::<String>,
                    synced_at => None::<String>,
                    missing => Vec::<String>::new(),
                },
                jobs => Vec::<crate::backfill::JobCtx>::new(),
                gaps => Vec::<crate::backfill::GapCtx>::new(),
                can_edit => false,
            })
            .expect("detail page renders");
        assert!(html.contains("3.46 V"), "battery in volts");
        assert!(!html.contains(">3458<"), "not the raw mV");
        assert!(
            html.contains(">pin<") && html.contains(">software<"),
            "reset cause named"
        );
        assert!(
            html.contains(">OK<") || html.contains("OK\n"),
            "status 0 reads OK"
        );
    }

    /// A payload the event did not carry leaves the field `None` rather than
    /// defaulting to zero — "crossed a bound of 0.0" would be a fabrication.
    #[test]
    fn a_missing_alarm_payload_leaves_the_numbers_absent() {
        let last = last_alarm_ctx(&[event(1, "threshold_alarm", None)]).expect("event");
        assert_eq!(last.field, None);
        assert_eq!(last.value, None);
        assert_eq!(last.threshold, None);
    }

    /// UX-037: the native alarm names its field `quantity`; the page needs
    /// it (and its unit) to say what the value was.
    #[test]
    fn a_native_alarm_names_its_quantity_and_unit() {
        let last = last_alarm_ctx(&[event(
            1,
            "alarm",
            Some(r#"{"quantity":"temperature","value":13.02,"type":"high"}"#),
        )])
        .expect("event");
        assert_eq!(last.field.as_deref(), Some("temperature"));
        assert_eq!(last.unit, Some("°C"));
    }

    /// devices.node.md §3.4: a field the Node never reported is not offered.
    /// A stored reading counts as reported, so the page survives a portal
    /// restart that emptied `node.fields`.
    #[test]
    fn node_detail_ctx_lists_only_reported_fields() {
        let mut n = node("aabbccdd00000001");
        n.fields.insert("temperature".to_string(), 21.5);
        let stored = std::collections::HashSet::from(["humidity".to_string()]);
        let ctx = node_detail_ctx(&n, None, None, None, Some(false), &stored);
        let names: Vec<&str> = ctx.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            ["temperature", "humidity"],
            "registry order, reported only"
        );
        let temp = &ctx.fields[0];
        assert_eq!(temp.value, Some(21.5));
        assert!(temp.thresholdable && temp.chartable);
        assert_eq!(
            ctx.fields[1].value, None,
            "stored but not live: no current value"
        );
    }

    fn config_entry(pairs: &[(&str, serde_json::Value)], w1: Option<&[&str]>) -> NodeConfigEntry {
        let mut config = serde_json::Map::new();
        // The boot config info always carries every cap_*; unset ones are false.
        for cap in [
            "cap_hall_left",
            "cap_hall_right",
            "cap_input_a",
            "cap_input_b",
            "cap_light_sensor",
            "cap_barometer",
            "cap_pir_detector",
            "cap_w1_sensors",
            "cap_accelerometer",
        ] {
            config.insert(format!("sensors.{cap}"), serde_json::Value::Bool(false));
        }
        for (k, v) in pairs {
            config.insert((*k).to_string(), v.clone());
        }
        NodeConfigEntry {
            config,
            synced_at: Some("2026-09-24T09:00:00Z".to_string()),
            w1_slot_type: w1.map(|t| t.iter().map(|s| s.to_string()).collect()),
            ..NodeConfigEntry::default()
        }
    }

    #[test]
    fn a_node_firmware_version_reads_from_either_document() {
        let mut device = DeviceState::new("h1".into());
        assert_eq!(node_fw_version(&device, "aa11"), None);
        device
            .node_infos
            .insert("aa11".into(), serde_json::json!({"fw_version": "v1.4.0"}));
        assert_eq!(node_fw_version(&device, "aa11"), Some((1, 4, 0)));
        device.node_device_infos.insert(
            "aa11".into(),
            NodeDeviceInfoEntry {
                received_at: "2026-10-12T08:00:00Z".into(),
                info: serde_json::json!({"fw_major": 1, "fw_minor": 5, "fw_patch": 0}),
            },
        );
        assert_eq!(node_fw_version(&device, "aa11"), Some((1, 5, 0)));
        assert_eq!(parse_fw_version("1.5.0-rc1"), Some((1, 5, 0)));
        assert_eq!(parse_fw_version("dev"), None);
    }

    /// MR !129 review: the reported list and the planned one compare as sets.
    #[test]
    fn history_channel_lists_compare_as_sets() {
        let pad = |n: usize| "ff".repeat(24 - n);
        let a = format!("000120{}", pad(3));
        let b = format!("200001{}", pad(3));
        assert!(same_history_channels(&a, &b), "order does not matter");
        assert!(same_history_channels(&a, &a.to_ascii_uppercase()));
        assert!(!same_history_channels(&a, &format!("0001{}", pad(2))));
        assert!(!same_history_channels(&a, "zz"), "not hex");
    }

    /// The History switch's channel list is what the Node measures (devices.node.md §3.9,
    /// amendment 2026-09-25; node-firmware-v2.0.0.md §6): the board's two first, then each
    /// declared field's `slot << 5 | ch` column, a 1-Wire slot by its type.
    #[test]
    fn the_history_channels_follow_what_the_node_measures() {
        let board = format!("0001{}", "ff".repeat(22));
        assert_eq!(
            history_channels(Some(&config_entry(&[], None)), None).as_deref(),
            Some(board.as_str())
        );
        assert_eq!(
            history_channels(None, None).as_deref(),
            Some(board.as_str()),
            "the board, whatever else is unknown"
        );
        let entry = config_entry(
            &[
                ("sensors.cap_barometer", serde_json::json!(true)),
                ("sensors.cap_hall_left", serde_json::json!(true)),
                ("sensors.cap_w1_sensors", serde_json::json!(true)),
            ],
            Some(&["dallas", "empty", "empty", "empty"]),
        );
        let all = format!("0001021506{}{}", "20", "ff".repeat(18));
        assert_eq!(
            history_channels(Some(&entry), None).as_deref(),
            Some(all.as_str()),
            "board, pressure, altitude, hall left count, s1 dallas temperature"
        );
    }

    /// #160 (node-firmware-v2.0.0.md §6, amendment 2026-10-28): a switched-off sensor is not
    /// recorded, the board's included; with none active there is no list.
    #[test]
    fn the_history_channels_leave_out_switched_off_sensors() {
        let mut node = crate::state::NodeState::new("aa11".into(), 0);
        node.disabled_fields.insert("humidity".into(), None);
        let entry = config_entry(&[("sensors.cap_barometer", serde_json::json!(true))], None);
        let full = history_channels(Some(&entry), None).unwrap();
        let active = history_channels(Some(&entry), Some(&node)).unwrap();
        assert!(!same_history_channels(&full, &active));
        assert!(!active.starts_with("0001"), "humidity left out: {active}");
        assert!(
            active.starts_with("00"),
            "board temperature still first: {active}"
        );
        assert_eq!(active.len(), 48);

        node.disabled_fields.insert("temperature".into(), None);
        assert_eq!(history_channels(None, Some(&node)), None, "nothing active");
    }

    #[test]
    fn the_history_switch_reads_the_draft_before_the_node() {
        let off = config_entry(
            &[("application.history_enable", serde_json::json!(false))],
            None,
        );
        let on = config_entry(
            &[("application.history_enable", serde_json::json!(true))],
            None,
        );
        assert!(
            !node_history_ctx(Some(&off), None, true).checked,
            "the Node keeps none"
        );
        assert!(
            node_history_ctx(Some(&off), Some(true), true).checked,
            "drafted on"
        );
        assert!(node_history_ctx(Some(&on), None, true).checked);
        assert!(
            !node_history_ctx(Some(&on), None, false).checked,
            "the download is off"
        );
        assert!(
            node_history_ctx(None, None, true).checked,
            "unknown is not off"
        );
        assert_eq!(
            node_history_ctx(Some(&on), None, true).node_enabled,
            Some(true)
        );
    }

    fn names(ctx: &NodeDetailCtx) -> Vec<&str> {
        ctx.fields.iter().map(|f| f.name.as_str()).collect()
    }

    /// devices.node.md §3.4 (dodatek 2026-09-24): without config info the board and what has
    /// arrived; never the whole registry, never a v1 field the Node did not send.
    #[test]
    fn without_config_info_the_board_and_what_arrived() {
        let mut n = node("aabbccdd00000001");
        n.fields.insert("temperature".to_string(), 21.5);
        n.fields.insert("pressure".to_string(), 1013.2);
        // The battery arrives on every uplink and is not a sensor (threshold_defaults.md §5).
        n.fields.insert("voltage".to_string(), 3.6);
        let ctx = node_detail_ctx(&n, None, None, None, Some(false), &Default::default());
        assert_eq!(names(&ctx), ["temperature", "humidity", "pressure"]);
        let temp = &ctx.fields[0];
        assert_eq!(temp.value, Some(21.5));
        assert!(temp.thresholdable && temp.chartable);
        assert_eq!(
            ctx.fields[1].value, None,
            "an active quantity without a value"
        );
        assert!(ctx.sensors.is_none(), "no config info, no sensor list");
    }

    /// The board only, as 0413 reports it: no cap_*, every 1-Wire slot empty.
    #[test]
    fn a_board_only_node_shows_the_board() {
        let n = node("aabbccdd00000001");
        let cfg = config_entry(&[], Some(&["empty", "empty", "empty", "empty"]));
        let ctx = node_detail_ctx(&n, None, None, Some(&cfg), Some(false), &Default::default());
        assert_eq!(names(&ctx), ["temperature", "humidity"]);
        let sensors = ctx.sensors.expect("a sensor list");
        assert!(sensors.caps.is_empty() && sensors.w1_slots.is_empty());
    }

    /// Each cap_* adds its fields in the table's order, and the 1-Wire slots follow, labelled
    /// with their type; a v1 field in the Node's state never shows.
    #[test]
    fn cap_sensors_and_one_wire_slots_follow_the_config_info() {
        let mut n = node("aabbccdd00000001");
        n.fields.insert("w1_1_temperature".to_string(), 21.5);
        n.fields.insert("ext_temperature_1".to_string(), 9.9);
        let cfg = config_entry(
            &[
                ("sensors.cap_barometer", serde_json::Value::Bool(true)),
                ("sensors.cap_gpio_input_a", serde_json::json!("pulse")),
                ("sensors.cap_gpio_input_b", serde_json::json!("analog")),
                ("sensors.cap_w1_sensors", serde_json::Value::Bool(true)),
            ],
            Some(&["dallas", "machine-probe", "empty", "type9"]),
        );
        let ctx = node_detail_ctx(&n, None, None, Some(&cfg), Some(false), &Default::default());
        assert_eq!(
            names(&ctx),
            [
                "temperature",
                "humidity",
                "pressure",
                "altitude",
                "input_a_count",
                "input_b_voltage",
                "w1_1_temperature",
                "w1_2_temperature",
                "w1_2_humidity",
                "w1_2_temperature_aux",
                "w1_2_illuminance",
                "w1_2_magnetic_field",
                "w1_2_tilt",
            ],
            "an unknown type with nothing reported adds nothing"
        );
        let dallas = ctx
            .fields
            .iter()
            .find(|f| f.name == "w1_1_temperature")
            .unwrap();
        assert_eq!(dallas.label, "1W slot 1 - Dallas - temperature");
        assert_eq!(dallas.value, Some(21.5));
        assert_eq!(dallas.unit, "\u{00b0}C");

        let sensors = ctx.sensors.expect("a sensor list");
        assert_eq!(
            sensors.caps,
            ["cap_barometer", "cap_input_a", "cap_input_b"],
            "a GP input in pulse counts, one in analog reports its voltage (v2.0.0)"
        );
        let slots: Vec<_> = sensors
            .w1_slots
            .iter()
            .map(|s| (s.slot, s.type_label.as_str()))
            .collect();
        assert_eq!(slots, [(1, "Dallas"), (2, "machine probe"), (4, "type9")]);
    }

    /// `cap_w1_sensors` off: the slot types are ignored, whatever they say.
    #[test]
    fn one_wire_slots_need_cap_w1_sensors() {
        let n = node("aabbccdd00000001");
        let cfg = config_entry(&[], Some(&["dallas", "empty", "empty", "empty"]));
        let ctx = node_detail_ctx(&n, None, None, Some(&cfg), Some(false), &Default::default());
        assert!(!names(&ctx).contains(&"w1_1_temperature"));
    }

    #[test]
    fn node_detail_ctx_per_field_alarm_state_is_not_normalized() {
        let mut n = node("aabbccdd00000001");
        n.fields.insert("temperature".to_string(), 21.5);
        n.field_alarm_states
            .insert("temperature".to_string(), "CRITICAL".to_string());
        let ctx = node_detail_ctx(&n, None, None, None, Some(false), &Default::default());
        let temp = ctx.fields.iter().find(|f| f.name == "temperature").unwrap();
        assert_eq!(temp.alarm_state.as_deref(), Some("CRITICAL"));
    }

    /// devices.node.md §5: a Node registered a minute ago has never
    /// reported, and the page has to say so rather than looking broken.
    /// Both arms of the ingest are covered, because the two say it
    /// differently — v1 leaves `last_seen` empty, v2 leaves
    /// `info.pairing_status` null until the first frame.
    #[test]
    fn a_registered_node_with_no_uplink_reads_as_waiting_for_join() {
        let fresh = node("70b3d5e050011234");
        assert!(fresh.last_seen.is_none(), "the v1 arm's shape");
        assert!(waiting_for_join(&fresh, None, false));
        // The v2 arm, before the first uplink flips it.
        assert!(waiting_for_join(
            &fresh,
            Some(&serde_json::json!({ "pairing_status": serde_json::Value::Null })),
            false
        ));
    }

    /// ...and stops saying so the moment either arm proves otherwise. A
    /// Node that has reported is joined whatever the inventory claims, and
    /// a Node the Hub calls "joined" is not waiting.
    #[test]
    fn a_joined_node_is_no_longer_waiting() {
        let mut reported = node("70b3d5e050011234");
        reported.last_seen = Some("2026-09-09T10:00:00Z".into());
        assert!(!waiting_for_join(&reported, None, false));
        assert!(!waiting_for_join(
            &reported,
            Some(&serde_json::json!({ "pairing_status": serde_json::Value::Null })),
            false
        ));

        let never_reported = node("70b3d5e050011234");
        assert!(!waiting_for_join(
            &never_reported,
            Some(&serde_json::json!({ "pairing_status": "joined" })),
            false
        ));
    }

    /// Waiting for a join is a LIFECYCLE state, not a fault. A fresh
    /// registration must not light the operational triangle on the whole
    /// list until its first uplink.
    #[test]
    fn waiting_for_join_is_not_an_operational_fault() {
        let fresh = node("70b3d5e050011234");
        let ctx = node_card_ctx(NodeCardInputs {
            offline: None,
            ..bare(&fresh)
        });
        assert!(ctx.waiting_for_join);
        assert!(
            ctx.fault.is_none(),
            "a Node doing exactly what it should is not faulty"
        );
    }

    /// §21 review, defect 2. A row that says `sending` long past any
    /// walk's budget belongs to a walk that no longer exists, and believing
    /// it leaves Send *and* Discard disabled with no way back short of a
    /// restart. The stored row is not touched — only the banner's reading
    /// of it.
    #[test]
    fn a_send_that_has_been_sending_impossibly_long_reads_as_an_error() {
        let mut row = pending_row(
            "application.interval_report",
            portal_storage::PENDING_KIND_CONFIG,
            "600",
        );
        row.state = portal_storage::PENDING_STATE_SENDING.into();
        row.updated_at = 1_000_000;

        let budget = (crate::pending::downlink_answer_budget(None)
            + crate::pending::SENDING_STALE_SLACK)
            .as_secs() as i64;

        // Inside the budget: a live walk, and the banner leaves it alone.
        let live = pending_ctx(
            "aa11",
            std::slice::from_ref(&row),
            None,
            &BTreeMap::new(),
            true,
            row.updated_at + budget - 1,
        );
        assert_eq!(live.rows[0].state, portal_storage::PENDING_STATE_SENDING);
        assert!(!live.rows[0].stale);
        assert!(live.sending, "Send and Discard stay disabled while it runs");

        // Past it: nothing can still be waiting, so the operator gets the
        // buttons back and a reason.
        let dead = pending_ctx(
            "aa11",
            std::slice::from_ref(&row),
            None,
            &BTreeMap::new(),
            true,
            row.updated_at + budget + 1,
        );
        assert_eq!(dead.rows[0].state, portal_storage::PENDING_STATE_ERROR);
        assert!(dead.rows[0].stale);
        assert!(!dead.sending, "the banner stops disabling both buttons");
        assert!(dead.failed, "and offers Send again");
    }

    /// §21 review, item 8. `NodeState::last_seen` is in-memory only, so
    /// after a restart every quiet Node looks like a fresh registration.
    /// The portal's own stored readings are the memory the store lost.
    #[test]
    fn a_node_with_stored_readings_is_not_waiting_for_a_join_after_a_restart() {
        // Exactly the post-restart shape: no `last_seen`, no inventory yet.
        let silent = node("70b3d5e050011234");
        assert!(
            waiting_for_join(&silent, None, false),
            "with nothing stored, a silent Node is indistinguishable from a new one"
        );
        assert!(
            !waiting_for_join(&silent, None, true),
            "a Node the portal has readings for has demonstrably joined"
        );

        // And through the row builder, where the flag actually reaches the
        // template.
        let ctx = node_card_ctx(NodeCardInputs {
            has_readings: true,
            ..bare(&silent)
        });
        assert!(!ctx.waiting_for_join);
    }

    /// The banner's key column shows what the operator edited, not the
    /// storage encoding — `threshold.temperature.critical_high` is a
    /// database key, "Temperature · critical_high" is a sentence.
    #[test]
    fn the_draft_banner_labels_a_threshold_bound_by_its_field() {
        let rows = vec![
            pending_row(
                "threshold.temperature.critical_high",
                portal_storage::PENDING_KIND_THRESHOLD,
                "9.5",
            ),
            pending_row(
                "application.interval_report",
                portal_storage::PENDING_KIND_CONFIG,
                "600",
            ),
        ];
        let mut thresholds = BTreeMap::new();
        thresholds.insert(
            "temperature".to_string(),
            ThresholdEntryCtx {
                critical_low: None,
                warning_low: None,
                warning_high: None,
                critical_high: Some(8.0),
                enabled: true,
                hysteresis: None,
            },
        );
        let mut entries = serde_json::Map::new();
        entries.insert("application.interval_report".into(), serde_json::json!(300));

        let ctx = pending_ctx("aa11", &rows, Some(&entries), &thresholds, true, NOW);
        let bound = &ctx.rows[0];
        assert!(
            bound.label.starts_with("Temperature"),
            "the field's label, not its slug: {}",
            bound.label
        );
        assert!(bound.label.ends_with("critical_high"));
        assert_eq!(bound.desired, "9.5", "the JSON scalar is unwrapped");
        assert_eq!(
            bound.current.as_deref(),
            Some("8"),
            "the merged thresholds say what the field holds now"
        );

        let config = &ctx.rows[1];
        assert_eq!(config.label, "application.interval_report");
        assert_eq!(config.current.as_deref(), Some("300"));
        assert_eq!(config.desired, "600");
    }

    /// A value the portal has never read is "unknown", not blank and not a
    /// zero. The distinction is the difference between "the device has no
    /// value" and "we have not asked".
    #[test]
    fn a_key_the_portal_has_never_read_has_no_current_value() {
        let rows = vec![pending_row(
            "application.interval_sample",
            portal_storage::PENDING_KIND_CONFIG,
            "60",
        )];
        let ctx = pending_ctx("aa11", &rows, None, &BTreeMap::new(), true, NOW);
        assert!(ctx.rows[0].current.is_none());
        assert!(!ctx.sending);
        assert!(!ctx.failed);
    }

    /// A cleared bound is drafted as JSON `null` and shown as a dash, not
    /// as the four characters `null`.
    #[test]
    fn a_cleared_bound_renders_as_unset() {
        let rows = vec![pending_row(
            "threshold.temperature.warning_low",
            portal_storage::PENDING_KIND_THRESHOLD,
            "null",
        )];
        let ctx = pending_ctx("aa11", &rows, None, &BTreeMap::new(), true, NOW);
        assert_eq!(ctx.rows[0].desired, "–");
    }

    /// P-3 (nodes_phase2.cs.md, dodatek 2026-09-25): the editor of a field the Node watches
    /// opens with both critical bounds preset to the open sides, so it cannot be saved
    /// one-sided by leaving a field blank; stored bounds, a field the Node does not watch and a
    /// Beacon's form are left as they are. Since threshold_defaults.md D5 a temperature or a
    /// humidity opens on its default band instead (the next test), so this is a quantity with
    /// no default, which the Node still watches.
    #[test]
    fn the_node_threshold_editor_presets_the_critical_band() {
        let templates = Templates::new("templates", "static");
        let render = |ctx: minijinja::Value| {
            templates
                .env()
                .get_template("partials/limits_editor.html")
                .expect("template loads")
                .render(ctx)
                .expect("renders")
        };
        let open = render(context! {
            lang => "en", can_edit => true, le_id => "t", le_post => "/nodes/aa11/thresholds/pressure", le_kind => "node", le_scope => "node", le_field => "pressure",
            le_unit => "hPa",
        });
        assert!(open.contains(r#"critical_low: "-99"#), "{open}");
        assert!(open.contains(r#"critical_high: "9999"#), "{open}");
        assert!(open.contains("A critical band is always a range"));

        let stored = render(context! {
            lang => "en", can_edit => true, le_id => "t", le_post => "/nodes/aa11/thresholds/temperature", le_kind => "node", le_scope => "node", le_field => "temperature",
            le_unit => "°C",
            t => context! { critical_low => 5.0, warning_low => (), warning_high => (),
                            critical_high => 25.0, enabled => true },
        });
        assert!(stored.contains(r#"critical_low: "5"#) && stored.contains(r#"critical_high: "25"#));

        let altitude = render(context! {
        lang => "en", can_edit => true, le_id => "t", le_post => "/nodes/aa11/thresholds/altitude", le_kind => "node", le_scope => "node", le_field => "altitude",
        });
        assert!(
            altitude.contains(r#"critical_low: """#),
            "not watched by the Node: {altitude}"
        );
        assert!(
            !altitude.contains("A critical band is always a range"),
            "and no range hint for it (NT-2)"
        );

        let beacon = render(context! {
        lang => "en", can_edit => true, le_id => "t", le_post => "/beacons/aa/thresholds/temperature", le_kind => "beacon", le_scope => "eye", le_field => "temperature",
        });
        // A Beacon opens on its default band, never on the Node's open sides.
        assert!(beacon.contains(r#"critical_low: "-10"#), "{beacon}");
        assert!(!beacon.contains(r#"critical_low: "-99"#));
        assert!(!beacon.contains("A critical band is always a range"));
    }

    /// alarm_hysteresis.md §5: the form has one Hystereze field, pre-filled with the field's
    /// own value, else its quantity's default (0.5 °C, 3 % RH).
    #[test]
    fn the_threshold_editor_prefills_the_hysteresis() {
        let templates = Templates::new("templates", "static");
        let render = |ctx: minijinja::Value| {
            templates
                .env()
                .get_template("partials/limits_editor.html")
                .expect("template loads")
                .render(ctx)
                .expect("renders")
        };
        let value = |html: &str| {
            let at = html.find(r#"name="hysteresis""#).expect("the field");
            let tail = &html[at..];
            let v = tail.find("value=\"").expect("a value") + 7;
            tail[v..v + tail[v..].find('"').expect("closed")].to_string()
        };
        let temp = render(context! {
            lang => "en", can_edit => true, le_id => "t", le_post => "/nodes/aa11/thresholds/temperature", le_kind => "node", le_scope => "node", le_field => "temperature",
            le_unit => "°C",
        });
        assert_eq!(value(&temp), "0.5");
        let hum = render(context! {
            lang => "en", can_edit => true, le_id => "t", le_post => "/beacons/AA/thresholds/humidity", le_kind => "beacon", le_scope => "eye", le_field => "humidity",
            le_unit => "%",
        });
        assert_eq!(value(&hum), "3.0");
        let own = render(context! {
            lang => "en", can_edit => true, le_id => "t", le_post => "/nodes/aa11/thresholds/temperature", le_kind => "node", le_scope => "node", le_field => "temperature",
            le_unit => "°C",
            t => context! { critical_low => (), warning_low => 2.0, warning_high => 6.0,
                            critical_high => (), enabled => true, hysteresis => 1.2 },
        });
        assert_eq!(value(&own), "1.2");
    }

    /// threshold_defaults.md D5 and the bench check of 2026-09-28: a field with no stored band
    /// opens on the configured default of its quantity with "Threshold enabled" UNCHECKED, on
    /// a Node and on a Beacon, so one Save arms nothing. A stored band shows its own switch:
    /// a default band, switched off, opens unchecked, one that is on opens checked. The slider
    /// seeds a blank band of a temperature or humidity unit from the same numbers.
    #[test]
    fn the_threshold_editor_opens_on_the_default_band_switched_off() {
        let templates = Templates::new("templates", "static");
        let render = |ctx: minijinja::Value| {
            templates
                .env()
                .get_template("partials/limits_editor.html")
                .expect("template loads")
                .render(ctx)
                .expect("renders")
        };
        let checked = |html: &str| {
            let at = html.find(r#"name="enabled""#).expect("the switch");
            html[at..at + html[at..].find('>').expect("closed")].contains("checked")
        };

        for (ctx, what) in [
            (
                context! {
                        lang => "en", can_edit => true, le_id => "t", le_post => "/nodes/aa11/thresholds/temperature", le_kind => "node", le_scope => "node", le_field => "temperature",
                le_unit => "°C",
                    },
                "a Node's temperature",
            ),
            (
                context! {
                        lang => "en", can_edit => true, le_id => "t", le_post => "/beacons/AA/thresholds/temperature", le_kind => "beacon", le_scope => "eye", le_field => "temperature",
                le_unit => "°C",
                    },
                "a Beacon's temperature",
            ),
        ] {
            let html = render(ctx);
            for (bound, value) in [
                ("critical_low", "-10"),
                ("warning_low", "0"),
                ("warning_high", "40"),
                ("critical_high", "50"),
            ] {
                assert!(
                    html.contains(&format!(r#"{bound}: "{value}"#)),
                    "{what}: {bound} {value}: {html}"
                );
            }
            assert!(!checked(&html), "{what} opens switched off: {html}");
            assert!(html.contains("nothing is evaluated or sent"), "{what}");
        }

        let humidity = render(context! {
            lang => "en", can_edit => true, le_id => "t", le_post => "/nodes/aa11/thresholds/w1_2_humidity", le_kind => "node", le_scope => "node", le_field => "w1_2_humidity",
            le_unit => "%",
        });
        assert!(humidity.contains(r#"warning_high: "80"#), "{humidity}");
        assert!(humidity.contains(r#"critical_high: "90"#), "{humidity}");

        let seeded_off = render(context! {
            lang => "en", can_edit => true, le_id => "t", le_post => "/nodes/aa11/thresholds/temperature", le_kind => "node", le_scope => "node", le_field => "temperature",
            le_unit => "°C",
            t => context! { critical_low => -10.0, warning_low => 0.0, warning_high => 40.0,
                            critical_high => 50.0, enabled => false },
        });
        assert!(!checked(&seeded_off), "{seeded_off}");
        let on = render(context! {
            lang => "en", can_edit => true, le_id => "t", le_post => "/nodes/aa11/thresholds/temperature", le_kind => "node", le_scope => "node", le_field => "temperature",
            le_unit => "°C",
            t => context! { critical_low => 5.0, warning_low => (), warning_high => (),
                            critical_high => 25.0, enabled => true },
        });
        assert!(checked(&on), "{on}");
        assert!(!on.contains("nothing is evaluated or sent"), "{on}");

        // The slider's seed: the configured band for °C, % and lx, none for a unit with none.
        let seed = |unit: &str| {
            let html = templates
                .env()
                .get_template("partials/threshold_slider.html")
                .expect("template loads")
                .render(context! { lang => "en", slider_unit => unit })
                .expect("renders");
            let at = html.find("seed: ").expect("a seed") + "seed: ".len();
            html[at..at + html[at..].find('\n').expect("line")]
                .trim()
                .to_string()
        };
        assert_eq!(seed("°C"), "[-10.0,0.0,40.0,50.0]");
        assert_eq!(seed("%"), "[0.0,10.0,80.0,90.0]");
        assert_eq!(seed("lx"), "[0.0,null,null,10000.0]");
        assert_eq!(seed("hPa"), "null");
    }

    /// The banner is an ALWAYS-PRESENT shell, like command_status.html: the
    /// first SSE frame after the first edit has to land somewhere, and a
    /// conditional shell would 404 it.
    #[test]
    fn the_draft_banner_renders_its_shell_with_no_draft() {
        let templates = Templates::new("templates", "static");
        let ctx = pending_ctx("aa11", &[], None, &BTreeMap::new(), true, NOW);
        let html = templates
            .env()
            .get_template("partials/pending_config.html")
            .expect("template loads")
            .render(context! { lang => "en", pending => ctx })
            .expect("renders");
        assert!(html.contains(r#"id="pending-config-aa11""#));
        assert!(html.contains(r#"sse-swap="pending-config-aa11""#));
        // innerHTML, so the element the SSE listener is bound to survives
        // every frame — see the partial's header.
        assert!(html.contains(r#"hx-swap="innerHTML""#));
        assert!(
            !html.contains("Send configuration"),
            "an empty draft offers nothing to send"
        );
    }

    /// A viewer sees the draft — it is what explains the Node's next
    /// behaviour — and is offered neither button.
    #[test]
    fn a_viewer_sees_the_banner_without_its_buttons() {
        let templates = Templates::new("templates", "static");
        let rows = vec![pending_row(
            "application.interval_report",
            portal_storage::PENDING_KIND_CONFIG,
            "600",
        )];

        for (can_edit, expect_buttons) in [(true, true), (false, false)] {
            let ctx = pending_ctx("aa11", &rows, None, &BTreeMap::new(), can_edit, NOW);
            let html = templates
                .env()
                .get_template("partials/pending_config_body.html")
                .expect("template loads")
                .render(context! { lang => "en", pending => ctx })
                .expect("renders")
                .replace("&#x2f;", "/");
            assert!(
                html.contains("application.interval_report"),
                "everyone sees the draft"
            );
            assert_eq!(
                html.contains("/nodes/aa11/pending/send"),
                expect_buttons,
                "can_edit = {can_edit}"
            );
            assert_eq!(html.contains("/nodes/aa11/pending/discard"), expect_buttons);
        }
    }

    /// Config sync Q20c, amended 2026-10-15: the awaiting banner keeps when and how often it
    /// was sent, and shows no deadline or time left; past the 4 h it says the fault is open.
    #[test]
    fn the_awaiting_banner_shows_no_countdown() {
        let templates = Templates::new("templates", "static");
        let rows = vec![pending_row(
            "application.interval_report",
            portal_storage::PENDING_KIND_CONFIG,
            "600",
        )];
        let render = |deadline_at: i64| {
            let d = portal_storage::NodeConfigDeadline {
                hostname: "hub1".into(),
                node_key: "aa11".into(),
                sent_at: NOW - 600,
                deadline_at,
                attempts: 2,
            };
            let ctx = PendingCtx {
                awaiting: Some(AwaitingCtx::of(&d, NOW)),
                ..pending_ctx("aa11", &rows, None, &BTreeMap::new(), true, NOW)
            };
            templates
                .env()
                .get_template("partials/pending_config_body.html")
                .expect("template loads")
                .render(context! { lang => "en", pending => ctx })
                .expect("renders")
                .replace("&#x2f;", "/")
        };
        let html = render(NOW + 3 * 3600);
        assert!(html.contains("data-awaiting"), "{html}");
        assert!(html.contains("Attempts: 2"), "{html}");
        assert!(!html.contains("has to confirm by"), "{html}");
        assert!(!html.contains(" left)"), "{html}");
        assert!(html.contains("application.interval_report"));
        assert!(html.contains("/nodes/aa11/pending/send"));
        assert!(render(NOW - 1).contains("A fault is open"));
    }

    /// #126 item 5: a draft Send would refuse says why on the banner and Send is disabled;
    /// the same draft without a refusal keeps Send enabled.
    #[test]
    fn a_draft_send_would_refuse_says_why_and_disables_send() {
        use crate::pending::SendRefusal;
        use crate::radio::alarm::TableRefusal;
        let templates = Templates::new("templates", "static");
        let rows = vec![pending_row(
            "threshold.w1_1_temperature.critical_high",
            portal_storage::PENDING_KIND_THRESHOLD,
            "30",
        )];
        let render = |refusal: Option<SendRefusal>| {
            let ctx = PendingCtx {
                refusal: refusal.as_ref().map(SendRefusalCtx::of),
                ..pending_ctx("aa11", &rows, None, &BTreeMap::new(), true, NOW)
            };
            templates
                .env()
                .get_template("partials/pending_config_body.html")
                .expect("template loads")
                .render(context! { lang => "en", pending => ctx })
                .expect("renders")
        };
        let send_button = |html: &str| -> String {
            let at = html.find("pending/send").expect("send button");
            let end = at + html[at..].find('>').expect("tag end");
            html[at..end].to_string()
        };

        let html = render(Some(SendRefusal::AlarmTable(TableRefusal::UnknownType(
            vec!["w1_1_temperature".into()],
        ))));
        assert!(html.contains("data-send-refusal"), "{html}");
        assert!(html.contains("no sensor type configured for the 1-Wire slot of w1_1_temperature"));
        assert!(html.contains("alert-error"));
        assert!(send_button(&html).contains("disabled"));

        let html = render(Some(SendRefusal::FirmwareTooOld {
            keys: vec!["alarms".into()],
            version: "1.4.0".into(),
        }));
        assert!(
            html.contains("alarms needs Node firmware 2.0.0 or newer, and this Node runs 1.4.0")
        );
        assert!(send_button(&html).contains("disabled"));

        let html = render(None);
        assert!(!html.contains("data-send-refusal"));
        assert!(!send_button(&html).contains("disabled"));
    }

    /// §3.6's card is admin-only in the template as well as on the route.
    /// A manager sees no card at all rather than disabled buttons.
    #[test]
    fn the_service_card_is_absent_for_a_manager_and_present_for_an_admin() {
        let templates = Templates::new("templates", "static");
        let render_for = |role: &str| {
            templates
                .env()
                .get_template("partials/node_service_card.html")
                .expect("template loads")
                .render(context! {
                    lang => "en",
                    n => context! { node_key => "aa11" },
                    raw_templates => raw_message_templates(None),
                    current_user => context! {
                        user_role => role,
                        is_superuser => false,
                    },
                })
                .expect("renders")
                .replace("&#x2f;", "/")
        };

        let manager = render_for("manager");
        assert!(
            manager.trim().is_empty(),
            "a manager gets no service card: {manager}"
        );

        let admin = render_for("admin");
        for path in [
            "/nodes/aa11/service/force-send",
            "/nodes/aa11/service/send-msg",
            "/nodes/aa11/service/reboot",
        ] {
            assert!(admin.contains(path), "{path} missing from the card");
        }
        for gone in ["/service/info", "/service/get-param", "/service/send-param"] {
            assert!(!admin.contains(gone), "{gone} left the card (WP6)");
        }
        // The raw message dialog says it goes at once, where the operator will read it.
        assert!(admin.contains("without a draft"));
        assert!(
            admin.contains("application.history_enable"),
            "a template key"
        );
        // The reboot asks inline (ui_components.md §7) and fills the route's
        // typed confirmation with the key; the server still checks it.
        assert!(admin.contains("Really restart aa11?"));
        assert!(admin.contains(r#"name="confirm" value="aa11""#));
        assert!(
            admin.contains("data-service-card"),
            "the shared, locked shell"
        );
    }

    /// PU-3: the uplink counter is one key for the template on both carriers — LoRaWAN's
    /// `fCnt`, P2P's `counter`.
    #[test]
    fn the_uplink_counter_reads_the_same_on_both_carriers() {
        for (carrier, key) in [
            (portal_domain::Transport::Lorawan, "fCnt"),
            (portal_domain::Transport::P2p, "counter"),
        ] {
            let mut n = node("aabbccdd00000001");
            n.transport = carrier;
            n.counters.insert(key.to_string(), 4217);
            let n = node_detail_ctx(
                &n,
                None,
                None,
                None,
                None,
                &std::collections::HashSet::from(["temperature".to_string()]),
            );
            assert_eq!(n.uplink_counter, Some(4217), "{carrier:?}");
        }
        assert_eq!(uplink_counter(&BTreeMap::new()), None);
    }

    /// P3: the session block (DevAddr, NetID, joined) follows the data, not the carrier: shown
    /// for any Node the Hub published a session for, and left out when it published none.
    #[test]
    fn the_session_block_shows_whenever_the_hub_published_a_session() {
        let templates = Templates::new("templates", "static");
        let render = |ctx: NodeDetailCtx| {
            templates
                .env()
                .get_template("pages/node_detail.html")
                .expect("template loads")
                .render(context! {
                    lang => "en",
                    current_user => None::<CurrentUser>,
                    hostname => "h1",
                    n => ctx,
                    active_tab => "diagnostics",
                    thresholds => BTreeMap::<String, ThresholdEntryCtx>::new(),
                    config => context! {
                        entries => None::<serde_json::Value>,
                        read_status => None::<String>,
                        synced_at => None::<String>,
                        missing => Vec::<String>::new(),
                    },
                    jobs => Vec::<crate::backfill::JobCtx>::new(),
                    gaps => Vec::<crate::backfill::GapCtx>::new(),
                    can_edit => false,
                })
                .expect("detail page renders")
        };
        for carrier in portal_domain::Transport::ALL {
            for published in [false, true] {
                let mut n = node("aabbccdd00000001");
                n.transport = carrier;
                if published {
                    n.dev_addr = Some("26011bda".into());
                    n.net_id = Some("000013".into());
                }
                let html = render(node_detail_ctx(
                    &n,
                    None,
                    None,
                    None,
                    Some(false),
                    &Default::default(),
                ));
                assert_eq!(
                    html.contains("26011bda"),
                    published,
                    "{carrier:?}, published = {published}"
                );
                assert_eq!(
                    html.contains(">NetID<"),
                    published,
                    "{carrier:?}, published = {published}"
                );
            }
        }
    }

    /// The draft's state vocabulary is rendered through a dynamic key
    /// (`("monitoring.pending.state_" ~ r.state)|t`), which
    /// `every_static_template_key_exists` cannot see — the same hole F9
    /// found on the fault tooltips. The list walked here is the storage
    /// layer's own, so a state added to the migration's CHECK without a
    /// translation fails this test rather than showing an operator a raw
    /// key.
    #[test]
    fn every_pending_state_key_exists_in_both_catalogues() {
        for state in portal_storage::PENDING_STATES {
            let key = format!("monitoring.pending.state_{state}");
            for lang in crate::i18n::LANGS {
                assert_ne!(
                    crate::i18n::lookup(lang, &key),
                    key,
                    "{key} is missing from the {lang} catalogue"
                );
            }
        }
    }

    /// `NodeCardCtx.fault` was computed by B2 and read by no template at all
    /// — devices.node.md gap D, the whole point of B2 on the Node list,
    /// rendered nowhere. The tooltip goes through the dynamic key
    /// `monitoring.nodes.fault_{fault}`, so this also proves the key
    /// resolves rather than falling back to itself.
    #[test]
    fn the_node_row_draws_a_fault_triangle_with_a_translated_tooltip() {
        let mut n = node("aabbccdd00000001");
        n.alarm_state = "NORMAL".to_string();
        let templates = Templates::new("templates", "static");

        let render = |ctx: NodeCardCtx| {
            templates
                .env()
                .get_template("partials/node_card.html")
                .expect("template loads")
                .render(context! { lang => "en", hostname => "h1", n => ctx })
                .expect("row renders")
        };

        let healthy = render(node_card_ctx(bare(&n)));
        assert!(!healthy.contains("data-fault"), "no fault, no indicator");

        let faulty = render(node_card_ctx(NodeCardInputs {
            offline: Some(true),
            ..bare(&n)
        }));
        assert!(
            faulty.contains(r#"data-fault="offline""#),
            "the row is marked faulty"
        );
        assert!(
            faulty.contains("text-warning"),
            "the indicator is drawn: {faulty}"
        );
        assert!(
            faulty.contains("Node is not reporting"),
            "the tooltip is translated, not the raw key: {faulty}"
        );
        assert!(
            !faulty.contains("monitoring.nodes.fault_"),
            "no raw key reaches the operator"
        );

        n.history_unavailable = true;
        let history = render(node_card_ctx(bare(&n)));
        assert!(history.contains("History could not be downloaded"));
        assert!(history.contains(r#"data-fault="history_unavailable""#));
    }

    /// calibration.cs.md §6 OQ 3: a certificate needing attention puts a
    /// calendar icon beside the name, tinted by its state (both warnings
    /// amber, only the expired one red — amendment 2026-10-13) and titled through
    /// the dynamic `monitoring.calibration.state_*` key — and it is not a
    /// fault: no triangle, no `data-fault`, the value state untouched.
    #[test]
    fn the_node_row_draws_a_calibration_icon_that_is_not_a_fault() {
        use crate::calibration::CalibrationLevel;
        let mut n = node("aabbccdd00000001");
        n.alarm_state = "NORMAL".to_string();
        let templates = Templates::new("templates", "static");
        let render = |ctx: NodeCardCtx| {
            templates
                .env()
                .get_template("partials/node_card.html")
                .expect("template loads")
                .render(context! { lang => "cs", hostname => "h1", n => ctx })
                .expect("row renders")
        };

        let valid = node_card_ctx(NodeCardInputs {
            calibration: Some(CalibrationLevel::Valid),
            ..bare(&n)
        });
        assert_eq!(valid.calibration, Some("valid"));
        let html = render(valid);
        assert!(html.contains("Kalibrace: Platná"), "{html}");
        assert!(html.contains("text-success"), "valid is green: {html}");

        let none = node_card_ctx(bare(&n));
        assert_eq!(none.calibration, None);
        let html = render(none);
        // The unified list draws no icon for a Node without a certificate
        // (devices.node.md, amendment 2026-10-14).
        assert!(!html.contains("data-calibration"), "no icon: {html}");

        for (level, label, tint) in [
            (
                CalibrationLevel::Warning,
                "Kalibrace: Brzy vyprší",
                "text-warning",
            ),
            (
                CalibrationLevel::SecondWarning,
                "Kalibrace: Vyprší do 7 dní",
                "text-warning",
            ),
            (
                CalibrationLevel::Expired,
                "Kalibrace: Prošlá",
                "text-critical",
            ),
        ] {
            let ctx = node_card_ctx(NodeCardInputs {
                calibration: Some(level),
                ..bare(&n)
            });
            assert_eq!(ctx.fault, None, "{level:?} is not a fault");
            assert_eq!(ctx.alarm_state, "Normal", "{level:?} leaves the value axis");
            let html = render(ctx);
            assert!(html.contains(label), "{level:?}: {html}");
            assert!(html.contains(tint), "{level:?}: {html}");
            assert!(!html.contains("data-fault"), "{level:?}");
            assert!(!html.contains("monitoring.calibration."), "no raw key");
        }
    }

    /// The §3.1b banner on the detail page: the fault kind, and for silence
    /// the "since when" the operator needs to judge it.
    #[test]
    fn the_node_detail_page_banners_the_fault_with_since_when() {
        let mut n = node("aabbccdd00000001");
        n.alarm_state = "NORMAL".to_string();
        n.last_seen = Some("2026-09-07T08:00:00Z".to_string());
        let templates = Templates::new("templates", "static");

        let render = |ctx: NodeDetailCtx| {
            templates
                .env()
                .get_template("pages/node_detail.html")
                .expect("template loads")
                .render(context! {
                    lang => "en",
                    current_user => None::<CurrentUser>,
                    hostname => "h1",
                    n => ctx,
                    thresholds => BTreeMap::<String, ThresholdEntryCtx>::new(),
                    config => context! {
                        entries => None::<serde_json::Value>,
                        read_status => None::<String>,
                        synced_at => None::<String>,
                        missing => Vec::<String>::new(),
                    },
                    jobs => Vec::<crate::backfill::JobCtx>::new(),
                    gaps => Vec::<crate::backfill::GapCtx>::new(),
                    can_edit => false,
                })
                .expect("detail page renders")
        };

        let healthy = render(node_detail_ctx(
            &n,
            None,
            None,
            None,
            Some(false),
            &Default::default(),
        ));
        assert!(!healthy.contains("Node is not reporting"));

        let offline = render(node_detail_ctx(
            &n,
            None,
            None,
            None,
            Some(true),
            &Default::default(),
        ));
        assert!(offline.contains("Node is not reporting"), "the banner");
        assert!(
            offline.contains("Last reported"),
            "the banner dates the fault"
        );

        n.history_unavailable = true;
        let history = render(node_detail_ctx(
            &n,
            None,
            None,
            None,
            Some(false),
            &Default::default(),
        ));
        assert!(history.contains("History could not be downloaded"));
        assert!(
            !history.contains("Last reported 2026"),
            "only silence carries a since-when"
        );
    }

    /// D3: the detail header's "is there a limit" follows the Monitoring
    /// rule ([`ThresholdEntryCtx::bounded`]): a switched-off limit set is no
    /// limit, so a Normal Node under one reads "no limit set".
    #[test]
    fn the_node_detail_header_ignores_a_switched_off_limit() {
        let mut n = node("aabbccdd00000001");
        n.alarm_state = "NORMAL".to_string();
        let templates = Templates::new("templates", "static");
        let render = |enabled: bool| {
            let thresholds = BTreeMap::from([(
                "temperature".to_string(),
                ThresholdEntryCtx {
                    critical_low: None,
                    warning_low: None,
                    warning_high: Some(30.0),
                    critical_high: None,
                    enabled,
                    hysteresis: None,
                },
            )]);
            templates
                .env()
                .get_template("pages/node_detail.html")
                .expect("template loads")
                .render(context! {
                    lang => "en",
                    current_user => None::<CurrentUser>,
                    hostname => "h1",
                    n => node_detail_ctx(&n, None, None, None, Some(false), &Default::default()),
                    has_limits => any_bounded(thresholds.values()),
                    thresholds => thresholds,
                    config => context! {
                        entries => None::<serde_json::Value>,
                        read_status => None::<String>,
                        synced_at => None::<String>,
                        missing => Vec::<String>::new(),
                    },
                    jobs => Vec::<crate::backfill::JobCtx>::new(),
                    gaps => Vec::<crate::backfill::GapCtx>::new(),
                    can_edit => false,
                })
                .expect("detail page renders")
        };
        let header = |html: &str| {
            // From the name to the transport badge beside the state badge.
            let start = html.find("<h1").expect("header");
            let end = start
                + html[start..]
                    .find("badge-outline-v1")
                    .expect("transport badge");
            html[start..end].to_string()
        };
        assert!(!header(&render(true)).contains("no limit set"));
        assert!(header(&render(false)).contains("no limit set"));
    }

    // ---- node_reported_state.md §4: the Node's own DeviceInfo and config --

    fn v2_device_info() -> NodeDeviceInfoEntry {
        NodeDeviceInfoEntry {
            received_at: "2026-09-22T19:09:04Z".to_string(),
            info: serde_json::json!({
                "fw_major": 1, "fw_minor": 4, "fw_patch": 0, "fw_version": "1.4.0",
                "build_type": "custom", "uptime_s": 18, "battery": 3458, "reset_cause": 3,
            }),
        }
    }

    /// On the v2 path `info` is the enrollment inventory and knows no firmware; the
    /// `device_info` document does, and it wins over whatever `info` says.
    #[test]
    fn the_firmware_column_reads_the_device_info_document_first() {
        let n = node("aabbccdd00000001");
        let inventory =
            serde_json::json!({"dev_eui": "aabbccdd00000001", "pairing_status": "joined"});
        let di = v2_device_info();

        let v2 = node_card_ctx(NodeCardInputs {
            info: Some(&inventory),
            device_info: Some(&di.info),
            ..bare(&n)
        });
        assert_eq!(v2.fw_version.as_deref(), Some("1.4.0"));

        let v1_info = serde_json::json!({"fw_version": "v1.3.9"});
        let v1 = node_card_ctx(NodeCardInputs {
            info: Some(&v1_info),
            ..bare(&n)
        });
        assert_eq!(
            v1.fw_version.as_deref(),
            Some("v1.3.9"),
            "the v1 fallback stands"
        );

        let neither = node_card_ctx(NodeCardInputs {
            info: Some(&inventory),
            ..bare(&n)
        });
        assert_eq!(neither.fw_version, None, "an inventory is not a DeviceInfo");
    }

    /// The detail card shows the `device_info` document with its age, the v1 `info` when that
    /// is all there is, and nothing — "no device info yet" — for a bare v2 inventory.
    #[test]
    fn the_detail_card_shows_the_nodes_own_info_and_never_the_inventory() {
        let n = node("aabbccdd00000001");
        let inventory = serde_json::json!({"dev_eui": "aabbccdd00000001", "pairing_status": null});
        let di = v2_device_info();

        let v2 = node_detail_ctx(
            &n,
            Some(&inventory),
            Some(&di),
            None,
            Some(false),
            &Default::default(),
        );
        assert!(v2.info.is_some());
        assert_eq!(v2.info_received_at.as_deref(), Some("2026-09-22T19:09:04Z"));
        assert!(
            v2.waiting_for_join,
            "pairing_status still comes from the inventory"
        );

        let bare_v2 = node_detail_ctx(
            &n,
            Some(&inventory),
            None,
            None,
            Some(false),
            &Default::default(),
        );
        assert!(
            bare_v2.info.is_none(),
            "the inventory is not rendered as DeviceInfo"
        );
        assert!(bare_v2.info_received_at.is_none());

        let v1_info = serde_json::json!({"fw_major": 1, "fw_minor": 3, "fw_patch": 9});
        let v1 = node_detail_ctx(
            &n,
            Some(&v1_info),
            None,
            None,
            Some(false),
            &Default::default(),
        );
        assert!(v1.info.is_some(), "the v1 DeviceInfo keeps rendering");
        assert!(v1.info_received_at.is_none());
    }

    #[test]
    fn the_diagnostics_tab_renders_the_device_info_with_its_age() {
        let n = node("aabbccdd00000001");
        let di = v2_device_info();
        let templates = Templates::new("templates", "static");
        let html = templates
            .env()
            .get_template("pages/node_detail.html")
            .expect("template loads")
            .render(context! {
                lang => "en",
                current_user => None::<CurrentUser>,
                hostname => "h1",
                active_tab => "diagnostics",
                n => node_detail_ctx(&n, None, Some(&di), None, Some(false), &Default::default()),
                thresholds => BTreeMap::<String, ThresholdEntryCtx>::new(),
                config => context! {
                    entries => None::<serde_json::Value>,
                    read_status => None::<String>,
                    synced_at => None::<String>,
                    missing => Vec::<String>::new(),
                },
                jobs => Vec::<crate::backfill::JobCtx>::new(),
                gaps => Vec::<crate::backfill::GapCtx>::new(),
                can_edit => false,
            })
            .expect("detail page renders");
        assert!(html.contains("1.4.0 (custom)"), "firmware row");
        assert!(html.contains("Reported"), "UI-9: the card says how old");
        assert!(!html.contains("No device info reported yet."));
    }

    #[test]
    fn the_config_card_lists_the_1wire_slots_read_only() {
        let templates = Templates::new("templates", "static");
        let html = templates
            .env()
            .get_template("partials/node_config_body.html")
            .expect("template loads")
            .render(context! {
                lang => "en",
                n => context! { node_key => "aabbccdd00000001" },
                can_edit => true,
                config => node_config_ctx(Some(&NodeConfigEntry {
                    config: serde_json::json!({"application.interval_report": 900})
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                    read_status: "complete".to_string(),
                    synced_at: Some("2026-09-22T19:09:09Z".to_string()),
                    w1_slot_type: Some(
                        ["machine-probe", "dallas", "empty", "type9"].map(String::from).to_vec(),
                    ),
                    ..NodeConfigEntry::default()
                })),
            })
            .expect("config body renders");
        assert!(html.contains("1-Wire slots"));
        assert!(html.contains("Slot 1") && html.contains("Slot 4"));
        assert!(html.contains("machine probe (DS28E17)"));
        assert!(html.contains("DS18B20 thermometer"));
        assert!(html.contains("type9"), "an unknown kind is shown as sent");
        assert!(
            !html.contains("node-cfg-aabbccdd00000001-w1_slot_type"),
            "a slot type is not an editable key"
        );
        assert!(
            !html.contains("monitoring.w1_slot."),
            "no raw key reaches the operator"
        );
    }

    // ---- nodes_phase1.cs.md WP5: the Configuration card -------------------

    fn config_of(pairs: serde_json::Value) -> NodeConfigEntry {
        NodeConfigEntry {
            config: pairs.as_object().cloned().unwrap_or_default(),
            read_status: "complete".to_string(),
            ..NodeConfigEntry::default()
        }
    }

    /// HIL 2026-10-14: every editable key's dialog (title, help) and every GP pin option
    /// label has a catalogue entry in both languages. The keys are built in the template, so
    /// the static template-key check cannot see them.
    #[test]
    fn every_config_edit_dialog_is_labelled() {
        use control_radio::decode::node_params::{node_param, ParamKind};
        let mut keys = Vec::new();
        for key in EDITABLE_CONFIG_KEYS {
            config_input(key).expect("an editable key has an input");
            let name = key.split_once('.').map_or(key, |(_, n)| n);
            keys.push(format!("monitoring.node_detail.config_param_{name}"));
            keys.push(format!("monitoring.node_detail.config_help_{name}"));
        }
        for key in LABELLED_ENUM_KEYS {
            let name = key.split_once('.').map_or(key, |(_, n)| n);
            let ParamKind::Enum(ref values) = node_param(key).expect("a GP key").kind else {
                panic!("{key} is an enum");
            };
            keys.extend(
                values
                    .iter()
                    .map(|(o, _)| format!("monitoring.node_detail.config_option_{name}_{o}")),
            );
        }
        for lang in ["en", "cs"] {
            for k in &keys {
                assert_ne!(crate::i18n::lookup(lang, k), *k, "{lang}: {k} missing");
            }
        }
    }

    /// Hynek 2026-10-14: the GP pin keys are read-only. With `can_edit` the settings tab has
    /// edit buttons for the intervals only, and the GP rows show the option label.
    #[test]
    fn gp_pin_keys_render_read_only() {
        for key in LABELLED_ENUM_KEYS {
            assert!(config_input(key).is_none(), "{key} must not be editable");
        }
        let templates = Templates::new("templates", "static");
        let n = node("aabbccdd00000001");
        for (lang, pulse) in [("en", "Pulse"), ("cs", "Pulzní")] {
            let html = templates
                .env()
                .get_template("pages/node_detail.html")
                .expect("template loads")
                .render(context! {
                    lang => lang,
                    current_user => None::<CurrentUser>,
                    hostname => "h1",
                    n => node_detail_ctx(&n, None, None, None, Some(false), &Default::default()),
                    has_limits => false,
                    thresholds => BTreeMap::<String, ThresholdEntryCtx>::new(),
                    config => node_config_ctx(Some(&config_of(serde_json::json!({
                        "application.interval_report": 900,
                        "application.interval_sample": 60,
                        "sensors.cap_gpio_mode": "inputs",
                        "sensors.cap_gpio_input_a": "pulse",
                        "sensors.cap_gpio_input_b": "none",
                    })))),
                    jobs => Vec::<crate::backfill::JobCtx>::new(),
                    gaps => Vec::<crate::backfill::GapCtx>::new(),
                    active_tab => "settings",
                    can_edit => true,
                })
                .expect("detail page renders");
            assert!(
                html.contains(r#"name="interval_report""#),
                "the Device configuration card: {html}"
            );
            for name in ["cap_gpio_mode", "cap_gpio_input_a", "cap_gpio_input_b"] {
                assert!(
                    !html.contains(&format!("node-cfg-aabbccdd00000001-sensors.{name}")),
                    "{lang}: {name} has an edit button or dialog"
                );
            }
            assert!(html.contains(pulse), "{lang}: the option label is shown");
            assert!(
                !html.contains("monitoring.node_detail.config_"),
                "{lang}: a raw key reaches the page"
            );
        }
    }

    #[test]
    fn config_info_shows_the_boot_settings_only() {
        // What the Hub may hold: the boot settings-info plus keys from an older full read
        // and from acked writes.
        let entry = config_of(serde_json::json!({
            "lorawan.region": "eu868",
            "lorawan.adr": true,
            "alarms.alarm_0": "03000102",
            "alarms.alarm_limit": 60,
            "sensors.cap_barometer": false,
            "sensors.cap_w1_sensors": true,
            "sensors.sensor1_rom": "28ff000000000000",
            "sensors.hall_left_counter": false,
            "application.calibration": false,
            "application.history_enable": false,
            "application.interval_sample": 60,
            "application.interval_report": 900,
        }));
        let ctx = node_config_ctx(Some(&entry));
        let groups = ctx.groups.expect("config info");
        let ids: Vec<&str> = groups.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, ["application", "sensors"]);
        let shown: Vec<&str> = groups
            .iter()
            .flat_map(|g| g.rows.iter().map(|r| r.key.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                "application.interval_sample",
                "application.interval_report",
                "application.history_enable",
                "sensors.cap_barometer",
                "sensors.cap_w1_sensors",
            ],
            "the settings-info keys in the firmware's order; nothing a full read left behind (N11)"
        );

        let editable: Vec<&str> = ctx.editable.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            editable,
            ["application.interval_sample", "application.interval_report"]
        );
        let sample = ctx.editable[0].edit.as_ref().expect("input");
        assert!(sample.zero_allowed);
        assert_eq!((sample.min, sample.max), (Some(5), Some(3600)));
        assert_eq!(sample.range.as_deref(), Some("0 or 5\u{2013}3600"));
        let report = ctx.editable[1].edit.as_ref().expect("input");
        assert_eq!(report.range.as_deref(), Some("60\u{2013}86400"));

        assert!(node_config_ctx(None).groups.is_none(), "never read");
    }

    /// nodes_phase1.cs.md WP6: the raw message's templates are the registry's LoRaWAN-writable
    /// keys minus the ones the portal owns or that are NFC only, valued from config info.
    #[test]
    fn raw_message_templates_offer_the_writable_keys_with_current_values() {
        let entry = config_of(serde_json::json!({
            "application.history_enable": true,
            "alarms.alarm_limit": 30,
        }));
        let templates = raw_message_templates(Some(&entry));
        let groups: Vec<&str> = templates.iter().map(|t| t.group.as_str()).collect();
        assert_eq!(groups, ["application", "sensors", "alarms"]);
        let keys: Vec<&str> = templates
            .iter()
            .flat_map(|t| t.keys.iter().map(|k| k.key.as_str()))
            .collect();
        for offered in [
            "application.calibration",
            "application.interval_report",
            "application.history_enable",
            "application.battery_level",
            "sensors.cap_barometer",
            "sensors.hall_left_counter",
            "sensors.accel_motion_sensitivity",
            "alarms.alarm_limit",
            "alarms.alarm_buzzer_mode",
            "alarms.radio_alarm_ack",
        ] {
            assert!(keys.contains(&offered), "{offered} offered: {keys:?}");
        }
        // One template for both carriers: every key offered, a downlink on either may write.
        for key in &keys {
            let param = control_radio::decode::node_params::node_params()
                .iter()
                .find(|p| p.full_name() == *key)
                .expect("a registry key");
            assert!(
                param.writable_over(control_radio::Transport::Lorawan)
                    && param.writable_over(control_radio::Transport::P2p),
                "{key} writable over both carriers"
            );
        }
        for refused in [
            "application.history_sensors",
            "application.history_channels",
            "application.vendor_reset_allow",
            "alarms.alarm_0",
            "sensors.sensor1_rom",
            "lorawan.region",
        ] {
            assert!(!keys.contains(&refused), "{refused} not offered");
            assert!(raw_message_key_refused(refused), "{refused} refused");
        }
        let application: serde_json::Value =
            serde_json::from_str(&templates[0].json).expect("json");
        assert_eq!(
            application["application.history_enable"], true,
            "current value"
        );
        assert_eq!(
            application["application.battery_level"], 2400,
            "firmware default"
        );
        let alarms: serde_json::Value = serde_json::from_str(&templates[2].json).expect("json");
        assert_eq!(
            alarms["alarms.alarm_limit"], 10,
            "not in the settings-info: the firmware default, not an older full read's 30"
        );
        assert_eq!(alarms["alarms.alarm_buzzer_mode"], "off");
        assert_eq!(
            alarms["alarms.radio_alarm_ack"], false,
            "sticker-firmware 3edb4a4: alarms unconfirmed unless set"
        );
        let buzzer = templates[2]
            .keys
            .iter()
            .find(|k| k.key == "alarms.alarm_buzzer_mode")
            .expect("buzzer");
        assert!(!buzzer.takes.contains("reserved"), "{}", buzzer.takes);
    }

    #[test]
    fn the_config_card_offers_an_edit_only_for_the_intervals() {
        let templates = Templates::new("templates", "static");
        let mut entry = config_of(serde_json::json!({
            "application.interval_report": 900,
            "application.history_enable": false,
            "alarms.alarm_0": "03000102",
        }));
        entry.synced_at = Some("2026-09-22T19:09:09.123456Z".to_string());
        let html = templates
            .env()
            .get_template("partials/node_config_body.html")
            .expect("template loads")
            .render(context! {
                lang => "en",
                n => context! { node_key => "aa11" },
                can_edit => true,
                config => node_config_ctx(Some(&entry)),
            })
            .expect("config body renders");
        assert!(html.contains("Application"), "the group heading");
        assert!(html.contains("history_enable") && html.contains("interval_report"));
        assert!(
            !html.contains("node-cfg-aa11-application.interval_report"),
            "the intervals are edited in the Device configuration card"
        );
        assert!(
            !html.contains("node-cfg-aa11-application.history_enable"),
            "read-only"
        );
        assert!(!html.contains("alarm_0"), "the portal owns the alarm slots");
        assert!(html.contains("reports by itself after every boot"));
        assert!(
            html.contains("Read settings"),
            "N10: the config card reads settings only"
        );
        assert!(
            html.contains("synced Sep 22, 19:09:09 UTC"),
            "N9: a readable UTC stamp, not raw RFC 3339: {html}"
        );
    }

    // ---- B5 (§8 WP5.2, §20): the Beacon row's fault and its detail link --

    /// `monitoring.nodes.fault_{fault}` is assembled in two templates, so
    /// `every_static_template_key_exists` cannot see it — the exact hole F9
    /// found on the monitoring side. Every variant `NodeFault` can serialise
    /// to must resolve in both catalogues.
    #[test]
    fn every_node_fault_key_exists_in_both_catalogues() {
        for fault in [
            NodeFault::Offline,
            NodeFault::HistoryUnavailable,
            NodeFault::MissingMeasurement,
        ] {
            let slug = serde_json::to_value(fault)
                .expect("serialises")
                .as_str()
                .expect("as a string")
                .to_string();
            let key = format!("monitoring.nodes.fault_{slug}");
            for lang in crate::i18n::LANGS {
                assert_ne!(
                    crate::i18n::lookup(lang, &key),
                    key,
                    "{key} is missing from the {lang} catalogue"
                );
            }
        }
    }

    /// WP7: the Device card assembles these keys from the firmware's tables, so
    /// `every_static_template_key_exists` cannot see them.
    #[test]
    fn every_device_card_key_exists_in_both_catalogues() {
        let mut keys: Vec<String> = Vec::new();
        for (_, bit) in DEVICE_STATUS_BITS {
            keys.push(format!("monitoring.node_status_bit.{bit}"));
            keys.push(format!("monitoring.node_status_bit.{bit}_help"));
        }
        keys.push("monitoring.alarm_source.slot".to_string());
        keys.push("monitoring.alarm_source.onboard".to_string());
        keys.extend(
            ALARM_TYPES
                .iter()
                .map(|t| format!("monitoring.alarm_type.{t}")),
        );
        for key in keys {
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
    fn the_device_card_names_status_bits_clock_and_active_alarms() {
        let templates = Templates::new("templates", "static");
        let mut n = node("aa11");
        n.fields.insert("temperature".to_string(), 21.5);
        let info = serde_json::json!({
            "fw_major": 2, "fw_minor": 0, "fw_patch": 0, "build_type": "dev", "debug": true,
            "serial_number": 33333, "unix_time": 1_790_244_003_u64,
            "device_status": (1 << 0) | (1 << 1) | (1 << 20),
            "active_alarms": [{"slot": 1, "channel": 0, "type": 2, "sensor_type": 2}],
        });
        let entry = NodeDeviceInfoEntry {
            info,
            received_at: "2026-09-24T10:00:00Z".to_string(),
        };
        let ctx = node_detail_ctx(
            &n,
            None,
            Some(&entry),
            None,
            None,
            &std::collections::HashSet::from(["temperature".to_string()]),
        );
        let html = templates
            .env()
            .get_template("pages/node_detail.html")
            .expect("template loads")
            .render(context! {
                lang => "en",
                active_tab => "diagnostics",
                hostname => "h1",
                n => ctx,
                thresholds => context! {},
                config => node_config_ctx(None),
                jobs => Vec::<crate::backfill::JobCtx>::new(),
                gaps => Vec::<crate::backfill::GapCtx>::new(),
                can_edit => true,
            })
            .expect("detail page renders");
        assert!(html.contains("Alarm active") && html.contains("Threshold alarm"));
        assert!(html.contains("bit20"), "an undefined bit as sent");
        assert!(html.contains("33333"), "serial number");
        assert!(html.contains("debug build"));
        assert!(html.contains("+3 s against the Hub"), "clock offset");
        let flat = html.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("1W slot 1 &middot; Temperature &middot; high"),
            "the active alarm by name"
        );
        assert!(
            html.contains("/nodes/aa11/read-device"),
            "the Device card's read button"
        );
        assert!(
            html.contains("Read info and settings"),
            "N10: says what it reads, unlike the config card's"
        );
    }

    // ---- GitLab #134: count rows; #135: the Node's last fault ---------------

    fn render_detail(ctx: NodeDetailCtx, tab: &str) -> String {
        Templates::new("templates", "static")
            .env()
            .get_template("pages/node_detail.html")
            .expect("template loads")
            .render(context! {
                lang => "en",
                current_user => None::<CurrentUser>,
                hostname => "h1",
                active_tab => tab,
                n => ctx,
                thresholds => BTreeMap::<String, ThresholdEntryCtx>::new(),
                config => context! {
                    entries => None::<serde_json::Value>,
                    read_status => None::<String>,
                    synced_at => None::<String>,
                    missing => Vec::<String>::new(),
                },
                jobs => Vec::<crate::backfill::JobCtx>::new(),
                gaps => Vec::<crate::backfill::GapCtx>::new(),
                can_edit => false,
            })
            .expect("detail page renders")
    }

    /// HIL 2026-10-14 (5722, STICKER 2.0.0): `input_a_count=0` and `input_b_count=0` arrived in
    /// `counters`, and the Motion and Input B rows read "–". Here on the GP inputs (PIR takes both
    /// pins, so it cannot sit next to input B): a pulse count of 0 on A, 12 on B.
    #[test]
    fn a_count_row_shows_the_latest_counter_value() {
        let mut n = node("aabbccdd00000001");
        n.fields.insert("temperature".into(), 21.5);
        n.counters.insert("input_a_count".into(), 0);
        n.counters.insert("input_b_count".into(), 12);
        n.counters.insert("counter".into(), 812);
        let entry = config_entry(
            &[
                ("sensors.cap_gpio_mode", serde_json::json!("inputs")),
                ("sensors.cap_gpio_input_a", serde_json::json!("pulse")),
                ("sensors.cap_gpio_input_b", serde_json::json!("digital")),
            ],
            None,
        );
        let ctx = node_detail_ctx(
            &n,
            None,
            None,
            Some(&entry),
            Some(false),
            &Default::default(),
        );
        let field = |name: &str| ctx.fields.iter().find(|f| f.name == name).expect(name);
        assert_eq!(field("input_a_count").value, Some(0.0), "0 is a value");
        assert_eq!(field("input_b_count").value, Some(12.0));
        assert!(field("input_a_count").count && field("input_a_count").chartable);
        assert!(!field("temperature").count);

        // The Channels card (ui_components.md §2) lists a count as a
        // Last-only row, as a whole number.
        let rd = crate::routes::channels::node_card(
            "ch".into(),
            "h1",
            &ctx,
            &Default::default(),
            &Default::default(),
            None,
            true,
            1,
        );
        let last = |label: &str| {
            rd.rows
                .iter()
                .find(|r| r.label.contains(label))
                .unwrap_or_else(|| panic!("{label} in the Channels card"))
        };
        assert_eq!(last("A").last.as_deref(), Some("0"), "0 is a value");
        assert_eq!(last("A").source, crate::routes::channels::Origin::Payload);
        assert!(last("A").action.is_some(), "a manager can switch a count");
        assert_eq!(last("B").last.as_deref(), Some("12"));
    }

    /// Without config info the Node lists what it has sent, and a count it has sent counts.
    #[test]
    fn a_sent_count_is_a_reported_field_without_config_info() {
        let mut n = node("aabbccdd00000001");
        n.counters.insert("hall_left_count".into(), 3);
        n.counters.insert("counter".into(), 9);
        let names: Vec<String> = active_node_fields(None, &n)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert!(names.contains(&"hall_left_count".to_string()), "{names:?}");
        assert!(
            !names.contains(&"counter".to_string()),
            "a frame counter is no sensor"
        );
    }

    fn faulted_info(received_at: &str) -> NodeDeviceInfoEntry {
        NodeDeviceInfoEntry {
            received_at: received_at.to_string(),
            info: serde_json::json!({
                "fw_major": 2, "fw_minor": 0, "fw_patch": 0, "reset_cause": 3,
                "fault_reason": 20, "fault_reason_name": "mem_instruction_access",
                "fault_pc": "0xf0000000", "fault_lr": "0x080280d5",
            }),
        }
    }

    /// HIL 2026-10-14: the forced HardFault's record on the Device card.
    #[test]
    fn the_device_card_shows_the_last_fault() {
        let n = node("aabbccdd00000001");
        let di = faulted_info("2026-10-14T08:00:00Z");
        let ctx = node_detail_ctx(&n, None, Some(&di), None, Some(false), &Default::default());
        assert_eq!(
            ctx.last_fault,
            Some(LastFaultCtx {
                reason: Some(20),
                reason_name: Some("mem_instruction_access".into()),
                pc: Some("0xf0000000".into()),
                lr: Some("0x080280d5".into()),
                received_at: Some("2026-10-14T08:00:00Z".into()),
            })
        );
        let html = render_detail(ctx, "diagnostics");
        assert!(html.contains("Last fault"));
        assert!(html.contains("mem_instruction_access (20)"));
        assert!(html.contains("0xf0000000") && html.contains("0x080280d5"));

        let clean = node_detail_ctx(
            &n,
            None,
            Some(&v2_device_info()),
            None,
            Some(false),
            &Default::default(),
        );
        assert!(clean.last_fault.is_none());
        assert!(!render_detail(clean, "diagnostics").contains("Last fault"));
    }

    /// The Node sends the record on one boot only: the stored one stands in once a later Info
    /// without it has replaced the document, and loses to a newer record in the document.
    #[test]
    fn a_stored_fault_outlives_the_info_that_carried_it() {
        let n = node("aabbccdd00000001");
        let record = fault_record(
            faulted_info("2026-10-14T08:00:00Z")
                .info
                .as_object()
                .unwrap(),
            "2026-10-14T08:00:00Z",
        )
        .expect("a record");
        assert_eq!(record["reset_cause"], 3);
        let stored = event(1_791_964_800, FATAL_ERROR_EVENT, Some(&record.to_string()));

        let later = NodeDeviceInfoEntry {
            received_at: "2026-10-14T09:00:00Z".into(),
            ..v2_device_info()
        };
        let ctx = node_detail_ctx(
            &n,
            None,
            Some(&later),
            None,
            Some(false),
            &Default::default(),
        )
        .with_last_fault(Some(&stored));
        let f = ctx.last_fault.expect("the stored record");
        assert_eq!(f.reason_name.as_deref(), Some("mem_instruction_access"));
        assert_eq!(f.received_at.as_deref(), Some("2026-10-14T08:00:00Z"));

        let newer = faulted_info("2026-10-14T10:00:00Z");
        let ctx = node_detail_ctx(
            &n,
            None,
            Some(&newer),
            None,
            Some(false),
            &Default::default(),
        )
        .with_last_fault(Some(&stored));
        assert_eq!(
            ctx.last_fault.and_then(|f| f.received_at).as_deref(),
            Some("2026-10-14T10:00:00Z")
        );

        assert!(fault_record(v2_device_info().info.as_object().unwrap(), "x").is_none());
        assert!(
            last_alarm_ctx(&[stored]).is_none(),
            "a crash record is not the Node's last alarm"
        );
    }
}
