//! The one sensor/channel picker shared by /trends and the monitoring group
//! form (partials/sensor_picker.html). Every page builds the same row shape
//! here; the template draws it the same way and per-page options only switch
//! what the row does (colour tag and live chart on Trends, locked rows and
//! form field names on the group form).

use serde::Serialize;

use crate::monitoring::{PickerDeviceCtx, PickerEntityCtx};
use crate::state::PortalState;

/// One channel row: a wired probe, or one quantity of a Node or Beacon.
#[derive(Debug, Clone, Serialize)]
pub struct SensorPickRow {
    /// The value the row selects (`?s=` token on Trends, `member[]` /
    /// `field[]` value on the group form).
    pub id: String,
    /// "probe" | "node" | "beacon" — the toolbar's kind filter.
    pub kind: &'static str,
    /// Registry field name, the Quantity filter (`""` when none yet).
    pub field: String,
    /// Device token rows are capped per (`data-select-per="owner"`).
    pub owner: String,
    /// The device's name.
    pub name: String,
    /// The quantity in the request's language ("Teplota").
    pub quantity: String,
    pub location: Option<String>,
    /// The Hub label the row belongs to.
    pub hub: String,
    pub value: Option<f64>,
    pub unit: String,
    pub connected: bool,
    pub selected: bool,
    /// Trends: the series colour of a selected row.
    pub color: Option<&'static str>,
    /// Group form: the group already holding this channel.
    pub locked_by: Option<String>,
    /// Group form: `field[]` or `member[]`; `None` posts nothing.
    pub input_name: Option<&'static str>,
}

fn lang_label(name: &str, label: &str) -> String {
    crate::i18n::field_label(crate::i18n::current_lang(), name, label)
}

/// The current value and unit of one Node field.
fn node_value(
    state: &PortalState,
    hostname: &str,
    key: &str,
    field: &str,
) -> (Option<f64>, String) {
    let Some(f) = fiber_protocol::lorawan_field(field) else {
        return (None, String::new());
    };
    let value = state
        .devices
        .get(hostname)
        .and_then(|d| d.nodes.get(key))
        .and_then(|n| n.fields.get(field))
        .map(|v| f.to_display(*v));
    (value, f.display_unit().unwrap_or("").to_string())
}

/// The current value and unit of one Beacon field.
fn beacon_value(
    state: &PortalState,
    hostname: &str,
    mac: &str,
    field: &str,
) -> (Option<f64>, String) {
    let b = state
        .devices
        .get(hostname)
        .and_then(|d| d.ble_sensors.get(mac));
    match field {
        "temperature" => (b.and_then(|b| b.temperature), "\u{b0}C".into()),
        "humidity" => (b.and_then(|b| b.humidity).map(|h| h as f64), "%".into()),
        _ => (None, String::new()),
    }
}

/// The group form's rows, from the claim-aware picker. `selected` holds whole
/// members (`probe:1`, legacy "all fields" members), `selected_fields` the
/// `token|field` channels. Wired probes of the first Hub only: a `probe:{n}`
/// token names no Hub and every member is stamped with the first one
/// (group_routes::current_hostname, decisions.md #8).
pub fn group_rows(
    state: &PortalState,
    picker: &[PickerDeviceCtx],
    selected: &[String],
    selected_fields: &[String],
) -> Vec<SensorPickRow> {
    let mut rows = Vec::new();
    for (i, d) in picker.iter().enumerate() {
        let probes: &[PickerEntityCtx] = if i == 0 { &d.probes } else { &[] };
        for e in probes.iter().chain(&d.nodes).chain(&d.beacons) {
            let any_field = e
                .fields
                .iter()
                .any(|f| selected_fields.contains(&format!("{}|{}", e.token, f.name)));
            let whole = selected.contains(&e.token) && !any_field;
            let base =
                |id: String, field: String, quantity: String, on: bool, holder: Option<&String>| {
                    let (value, unit) = match e.kind {
                        "probe" => {
                            let line = e.token.trim_start_matches("probe:").parse::<u8>().ok();
                            let v = state
                                .devices
                                .get(&d.hostname)
                                .and_then(|dev| dev.sensors.iter().find(|s| Some(s.line) == line))
                                .and_then(|s| s.temperature_avg_celsius);
                            (v, "\u{b0}C".to_string())
                        }
                        "node" => node_value(
                            state,
                            &d.hostname,
                            e.token.trim_start_matches("node:"),
                            &field,
                        ),
                        _ => beacon_value(
                            state,
                            &d.hostname,
                            e.token.trim_start_matches("beacon:"),
                            &field,
                        ),
                    };
                    SensorPickRow {
                        input_name: None,
                        kind: e.kind,
                        owner: e.token.clone(),
                        name: e.name.clone(),
                        quantity,
                        location: e.location.clone(),
                        hub: d.label.clone(),
                        value,
                        unit,
                        connected: e.connected,
                        selected: on,
                        color: None,
                        locked_by: holder.filter(|_| !on).cloned(),
                        id,
                        field,
                    }
                };
            if e.fields.is_empty() {
                let quantity = if e.kind == "probe" {
                    lang_label("temperature", "Temperature")
                } else {
                    e.detail.clone().unwrap_or_default()
                };
                let field = if e.kind == "probe" { "temperature" } else { "" };
                let mut row = base(
                    e.token.clone(),
                    field.into(),
                    quantity,
                    selected.contains(&e.token),
                    e.in_group.as_ref(),
                );
                row.input_name = Some("member[]");
                rows.push(row);
                continue;
            }
            for f in &e.fields {
                let id = format!("{}|{}", e.token, f.name);
                let on = selected_fields.contains(&id) || whole;
                let holder = f.in_group.as_ref().or(e.in_group.as_ref());
                let mut row = base(
                    id,
                    f.name.clone(),
                    lang_label(&f.name, &f.label),
                    on,
                    holder,
                );
                row.input_name = Some("field[]");
                rows.push(row);
            }
        }
    }
    rows
}

/// The Quantity filter's options on lists that span every device kind
/// (/alarms): every thresholdable, continuous quantity a Node or Beacon can
/// report, labelled in the request's language.
pub fn quantity_options() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for f in fiber_protocol::lorawan_registry() {
        if f.kind == fiber_protocol::LorawanFieldKind::Continuous
            && f.thresholdable
            && f.group != fiber_protocol::LorawanFieldGroup::Battery
            && !out.iter().any(|(n, _)| n == f.name)
        {
            out.push((f.name.to_string(), lang_label(f.name, f.label)));
        }
    }
    out
}
