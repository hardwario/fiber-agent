use super::*;

#[derive(Debug, Clone, Serialize)]
pub struct MqttCtx {
    pub connected: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StandbyCtx {
    pub active: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SensorCardCtx {
    pub hostname: String,
    pub line: u8,
    pub name: Option<String>,
    pub location: Option<String>,
    pub connected: bool,
    pub avg_c: Option<f64>,
    pub min_c: Option<f64>,
    pub max_c: Option<f64>,
    pub dominant_alarm_state: String,
    pub alarm_triggered_at: Option<String>,
    pub updated_at: Option<String>,
    /// M4-D additions — see `partials/hub_sensor_row.html`'s header comment.
    pub can_edit: bool,
    pub thresholds: Option<ThresholdsCtx>,
    /// The calibration icon beside the line number (calibration.cs.md, UI
    /// amendment 2026-10-14): the level, `None` = no certificate (grey).
    /// Set by the routes, which hold the levels.
    pub calibration: Option<&'static str>,
    /// The current certificate's expiry date, RFC 3339 UTC midnight — the
    /// row's "do {datum}"; `None` without a certificate.
    pub calibration_until: Option<String>,
}

/// `partials/limits_editor.html`'s `t` context and `GET /api/v1/chart/{line}`'s
/// `thresholds` object share this exact shape — field names match
/// `state::SensorThresholds` (see that struct's doc-comment).
/// `routes::history_routes` keeps its own identical `ThresholdsJson` for the
/// chart endpoint rather than depending on this module for it; duplicated
/// here for the same reason (small, no cross-module coupling gained).
#[derive(Debug, Clone, Serialize)]
pub struct ThresholdsCtx {
    pub critical_low: Option<f64>,
    pub warning_low: Option<f64>,
    pub warning_high: Option<f64>,
    pub critical_high: Option<f64>,
    pub alarm_low: Option<f64>,
    pub alarm_high: Option<f64>,
    /// What the line's form pre-fills (alarm_hysteresis.md §5): the Hub's echo of the
    /// portal's value; `None` = the temperature default.
    pub hysteresis: Option<f64>,
    /// The band's switch (#167); a switched-off band draws nothing.
    pub enabled: bool,
}

impl From<&SensorThresholds> for ThresholdsCtx {
    fn from(t: &SensorThresholds) -> Self {
        ThresholdsCtx {
            critical_low: t.critical_low,
            warning_low: t.warning_low,
            warning_high: t.warning_high,
            critical_high: t.critical_high,
            alarm_low: t.alarm_low,
            alarm_high: t.alarm_high,
            hysteresis: t.hysteresis,
            enabled: t.is_enabled(),
        }
    }
}

/// Same shape `device_status.html` and dashboard.html's `device` key share,
/// plus `sensors` (which only the dashboard page context uses — the
/// `device_status.html` partial simply ignores the extra field).
#[derive(Debug, Clone, Serialize)]
pub struct DeviceCtx {
    pub hostname: String,
    pub online: bool,
    pub version: Option<String>,
    /// `version` with a single leading "v" — pages/hub_detail.html's header
    /// comment documents the pre-existing bug this fixes: naively
    /// prepending "v" in the template double-prefixed a firmware version
    /// that already starts with one (mock data ships as "v0.99.0-mock").
    pub display_version: Option<String>,
    pub last_seen: Option<String>,
    pub standby: StandbyCtx,
    pub sensors: Vec<SensorCardCtx>,
}

/// `device.version` -> a single leading "v", regardless of whether the
/// stored value already has one. See [`DeviceCtx::display_version`].
fn display_version(version: Option<&str>) -> Option<String> {
    version.map(|v| {
        if v.starts_with('v') || v.starts_with('V') {
            v.to_string()
        } else {
            format!("v{v}")
        }
    })
}

/// Firmware-cased -> template mixed-case, with the "never actually
/// reported" fallback documented in the module doc.
fn normalize_alarm_state(s: &SensorState) -> String {
    let never_updated = s.updated_at.is_none() && s.sample_count == 0 && !s.connected;
    if never_updated {
        return "NeverConnected".to_string();
    }
    match s.dominant_alarm_state.as_str() {
        "NORMAL" => "Normal",
        "WARNING" => "Warning",
        "CRITICAL" => "Critical",
        "DISCONNECTED" => "Disconnected",
        "RECONNECTING" => "Reconnecting",
        "NEVERCONNECTED" => "NeverConnected",
        // Unrecognized firmware string: pass through verbatim rather than
        // guess — sensor_card.html's if/elif chain falls into its ghost/
        // muted else-branch for anything it doesn't recognize, so this
        // degrades safely rather than panicking or lying about state.
        other => other,
    }
    .to_string()
}

pub fn mqtt_ctx(mqtt: &MqttConnectionState) -> MqttCtx {
    MqttCtx {
        connected: mqtt.connected,
        reason: mqtt.last_disconnect_reason.clone(),
    }
}

pub fn sensor_card_ctx(hostname: &str, s: &SensorState, can_edit: bool) -> SensorCardCtx {
    SensorCardCtx {
        hostname: hostname.to_string(),
        line: s.line,
        name: s.name.clone(),
        location: s.location.clone(),
        connected: s.connected,
        avg_c: s.temperature_avg_celsius,
        min_c: s.temperature_min_celsius,
        max_c: s.temperature_max_celsius,
        dominant_alarm_state: normalize_alarm_state(s),
        alarm_triggered_at: s.alarm_triggered_at.clone(),
        updated_at: s.updated_at.map(format_time),
        can_edit,
        thresholds: s.thresholds.as_ref().map(ThresholdsCtx::from),
        calibration: None,
        calibration_until: None,
    }
}

pub fn device_status_ctx(device: &DeviceState, can_edit: bool) -> DeviceCtx {
    DeviceCtx {
        hostname: device.hostname.clone(),
        online: device.online,
        version: device.version.clone(),
        display_version: display_version(device.version.as_deref()),
        last_seen: device.last_seen.map(format_time),
        standby: StandbyCtx {
            active: device.standby.active,
            reason: device.standby.reason.clone(),
        },
        sensors: device
            .sensors
            .iter()
            .map(|s| sensor_card_ctx(&device.hostname, s, can_edit))
            .collect(),
    }
}

/// `local_device` is the caller's local-vs-remote verdict
/// (`routes::hub_routes::is_local_device`): the device on the bus is the
/// machine this portal runs on, so the pair form drops its code input.
///
/// `pages/hub_detail.html`'s context, minus the `error`/`ok` flash fields
/// (`routes::hub_routes::hub_detail`'s job — they come from the query
/// string, not the store). `hostname`/`online`/`version` read off the store
/// the same "phase-1 first device" way `dashboard_ctx` does.
///
/// `device_label` prefers `SystemState::device_label` (refreshed on every
/// `system/info`) and falls back to `DeviceConfigState::device_label`
/// (refreshed on `config/state`) for a device that has one set but hasn't
/// sent a fresh `system/info` yet — same two sources `state::models`'
/// doc-comment describes, neither is more "authoritative" than the other,
/// they're just refreshed by different messages.
///
/// `paired_via_invite` says the pairing was started by the device itself and has no portal user
/// behind it (GitLab #61) — the template renders that instead of naming a user, because
/// `paired_by` is `None` both for "nobody signed in for this" and for "the record is gone", and
/// those read the same on screen.
pub fn device_page_ctx(
    device: Option<&DeviceState>,
    pairing: Option<&DeviceIdentityInfo>,
    paired_by_username: Option<String>,
    paired_via_invite: bool,
    local_device: bool,
) -> Value {
    let hostname = device.map(|d| d.hostname.clone());
    let online = device.map(|d| d.online).unwrap_or(false);
    let version = device.and_then(|d| d.version.clone());
    let device_label = device.and_then(|d| {
        d.system
            .device_label
            .clone()
            .or_else(|| d.config.device_label.clone())
    });

    let pairing_ctx = match pairing {
        Some(info) => PairingCtx {
            paired: true,
            ca_fingerprint: Some(ca_fingerprint(&info.ca_public_key)),
            paired_by: paired_by_username,
            paired_via_invite,
            paired_at: Some(info.paired_at.clone()),
        },
        None => PairingCtx {
            paired: false,
            ca_fingerprint: None,
            paired_by: None,
            paired_via_invite: false,
            paired_at: None,
        },
    };

    context! {
        hostname => hostname,
        online => online,
        device_label => device_label,
        version => version,
        pairing => pairing_ctx,
        local_device => local_device,
    }
}

/// `pages/hub_detail.html`'s `pairing` context.
#[derive(Debug, Clone, Serialize)]
pub struct PairingCtx {
    pub paired: bool,
    pub ca_fingerprint: Option<String>,
    pub paired_by: Option<String>,
    /// No user paired this Hub — the Hub asked for it itself over `pair/invite`.
    pub paired_via_invite: bool,
    pub paired_at: Option<String>,
}

/// Portal-local display fingerprint for a device CA public key: first 16
/// hex chars of `SHA-256(public_key_hex as ASCII)` — same convention
/// `keys::service::fingerprint_of_hex_key` uses for per-user key
/// fingerprints (that function is private to its module; duplicated here
/// rather than exported, it's three lines and this is the only other
/// caller).
fn ca_fingerprint(ca_public_key_hex: &str) -> String {
    let digest = Sha256::digest(ca_public_key_hex.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        // Writing to a String is infallible.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// One row of `pages/hubs.html` (devices.hub.md §2.3), built from the
/// store alone. Honest about what the Hub has not reported: the serial comes
/// from `system/info.hardware` and the gateway EUI from `system/info.lorawan`,
/// and either stays `None` — the template prints "—" — until a Hub that
/// publishes it has, rather than inventing a value.
#[derive(Debug, Clone, Serialize)]
pub struct HubRowCtx {
    pub hostname: String,
    /// `device_label`, from either of the two blocks that refresh it — the
    /// same preference [`device_page_ctx`] uses.
    pub name: Option<String>,
    /// The full CPU serial, `system/info.hardware.serial` (§3.1, amendment
    /// 2026-09-28).
    pub serial: Option<String>,
    /// The operator's location, `device_meta.location` (metadata.cs.md); the
    /// Hub itself reports none.
    pub location: Option<String>,
    /// The operator's description, `device_meta.description`: not shown on
    /// the row, but the list's fulltext search matches it (B1).
    pub description: Option<String>,
    pub online: bool,
    /// RFC 3339, so the row can render it through the `short_dt` filter and
    /// keep the absolute value in a tooltip.
    pub last_seen: Option<String>,
    pub display_version: Option<String>,
    /// From `system/info.lorawan` (§12 A); `None` until the Hub has reported it.
    pub gateway_eui: Option<String>,
    /// The Radio cell (§2.3): service health chips, `None` until the first
    /// `system/info.lorawan` block.
    pub radio: Option<HubRadioCtx>,
    /// This is the Hub the portal runs on and the portal's own radio seam
    /// is inert, so no command reaches its Nodes from here — whatever the
    /// green chips from `system/info.lorawan` say. The route sets it; see
    /// `hub_routes::radio_control_off`.
    pub radio_control_off: bool,
    /// The Hub's battery, "x %" for the Overview cell like a Node or Beacon (#181).
    pub battery: Option<String>,
    /// The battery icon's tone: `"critical"` below 10 %, `"warning"` below 20 %, else `""`.
    pub battery_tone: &'static str,
    /// No pairing is stored for this Hub, so no signed command reaches it
    /// (UX-035). The route sets it from the key store.
    pub unpaired: bool,
    /// The `radio/lorawan` document says `disabled`: no LoRaWAN chips, which would
    /// read as a missing radio on a P2P Hub (PN-2), same rule as the Hub card.
    pub lorawan_disabled: bool,
    /// The Northbridge's state from its radio diagnostics document; `None` when the
    /// Hub publishes none or P2P is off.
    pub northbridge: Option<NorthbridgeCardCtx>,
    /// The Hub's value axis (§1): the worst class any of its eight wired
    /// lines is in. Drives the row's left strip and the list's `oor` filter.
    pub value_class: &'static str,
    /// The operational axis (§2.3), kept apart from the value one: offline,
    /// or any Hub-scoped `SystemAlertKind` the notifier holds on ([`hub_fault`]).
    pub fault: bool,
    /// What the fault triangle says, as i18n keys: offline first, then each
    /// condition on — so a Hub that is online but low on storage does not
    /// read as offline.
    pub fault_keys: Vec<String>,
    /// The triangle is red: DC input missing or battery below 10 % (#181).
    pub fault_critical: bool,
    /// `"server"` for the Hub this portal runs on, `"client"` for a
    /// registered Client Hub (§2.2's Typ), `None` for neither — a Hub on the
    /// broker that is in no registry (OQ 18). The route sets it.
    pub kind: Option<&'static str>,
    /// The Client's registry row (§2.5, §12 J); `None` for every other Hub.
    pub cluster: Option<ClusterRowCtx>,
    /// The most urgent calibration state among this Hub's probes needing
    /// attention (`warning`/`second_warning`/`expired`), for the calendar
    /// icon beside the name (calibration.cs.md amendment 2026-10-14). Set
    /// by the route, which holds the levels.
    pub calibration: Option<&'static str>,
    /// The Overview's second line in the unified device table
    /// (devices.hub.md, amendment 2026-10-14): Ethernet first, then Wi-Fi.
    pub ip: Option<String>,
    /// The Values lines (amendment 2026-10-14): the highest, then the lowest
    /// current reading across the Hub's connected wired probes. Empty
    /// without a reading.
    pub fields: Vec<HubRowFieldCtx>,
}

/// One Values line, in the `{name, label, unit, value, alarm_state}` shape
/// Node and Beacon fields share.
#[derive(Debug, Clone, Serialize)]
pub struct HubRowFieldCtx {
    pub name: String,
    pub label: String,
    pub unit: &'static str,
    pub value: Option<f64>,
    /// The probe's state, firmware casing; the template compares
    /// case-insensitively.
    pub alarm_state: String,
}

/// [`HubRowCtx::fields`]: max then min of the connected probes' readings.
fn hub_row_fields(device: &DeviceState) -> Vec<HubRowFieldCtx> {
    let read: Vec<_> = device
        .sensors
        .iter()
        .filter(|s| s.connected)
        .filter_map(|s| s.temperature_avg_celsius.map(|v| (v, s)))
        .collect();
    let line = |(v, s): (f64, &SensorState)| HubRowFieldCtx {
        name: format!("line{}", s.line),
        label: s.name.clone().unwrap_or_else(|| format!("#{}", s.line)),
        unit: "°C",
        value: Some(v),
        alarm_state: s.dominant_alarm_state.to_ascii_uppercase(),
    };
    let max = read.iter().copied().max_by(|a, b| a.0.total_cmp(&b.0));
    let min = read.iter().copied().min_by(|a, b| a.0.total_cmp(&b.0));
    max.into_iter().chain(min).map(line).collect()
}

/// One registered Client Hub, as the Hubs list (§2.5) and the Server's
/// cluster card (§3.4) show it. Built by `routes::cluster_routes`; nothing
/// here can hold the Client's password, which the registry does not keep.
#[derive(Debug, Clone, Serialize)]
pub struct ClusterRowCtx {
    pub hostname: String,
    pub gateway_eui: Option<String>,
    pub peer_username: String,
    /// `"pending"` | `"failed"` | `"complete"`.
    pub state: &'static str,
    pub registration_error: Option<String>,
    pub removal_error: Option<String>,
    pub registered_at: String,
    pub registered_by: Option<String>,
    pub online: bool,
    /// No `DeviceState` yet: "registrováno — čeká na připojení" (§2.5).
    pub never_connected: bool,
    /// The Client runs a LoRaWAN gateway its registration does not let in
    /// (`cluster::gateway_gap`, cluster.md amendment 2026-10-13).
    pub gateway_gap: Option<crate::cluster::GatewayGap>,
    /// The radio mode the Client last reported (`lorawan.mode`), when it did.
    pub radio_mode: Option<String>,
    /// That mode is not the Server's: the follower has not converged it yet (decision #47).
    pub radio_mode_differs: bool,
}

/// The list row of a Client that is in the registry and not yet in the
/// store (§2.5 "Klient registrován, nikdy nepřipojen"): what the registry
/// knows, "—" for the rest, never a fault — it has not failed, it has not
/// arrived.
pub fn registered_hub_row_ctx(
    row: ClusterRowCtx,
    meta: Option<&portal_storage::DeviceMeta>,
) -> HubRowCtx {
    HubRowCtx {
        hostname: row.hostname.clone(),
        name: meta.and_then(|m| m.name.clone()),
        serial: None,
        location: meta.and_then(|m| m.location.clone()),
        description: meta.and_then(|m| m.description.clone()),
        online: false,
        last_seen: None,
        display_version: None,
        gateway_eui: row.gateway_eui.clone(),
        radio: None,
        battery: None,
        battery_tone: "",
        lorawan_disabled: false,
        northbridge: None,
        radio_control_off: false,
        unpaired: false,
        value_class: "normal",
        fault: false,
        fault_keys: Vec::new(),
        fault_critical: false,
        kind: Some("client"),
        cluster: Some(row),
        calibration: None,
        ip: None,
        fields: Vec::new(),
    }
}

/// A Hub's value axis: the worst class any of its eight wired lines is in,
/// in the row's `data-oor` spelling (devices.hub.md §1, §2.1). Shared with
/// `page_stats_ctx`, so the "Mimo meze" tile counts the rows that carry the
/// class rather than a second opinion about them.
pub fn hub_value_class(device: &DeviceState) -> &'static str {
    let mut class = "normal";
    for sensor in &device.sensors {
        match sensor.dominant_alarm_state.to_ascii_uppercase().as_str() {
            "CRITICAL" => class = "critical",
            "WARNING" if class != "critical" => class = "warning",
            _ => {}
        }
    }
    class
}

/// A Hub's operational axis: whether its row draws the fault triangle
/// (devices.hub.md §2.1) — offline, or any Hub-scoped `SystemAlertKind` the
/// notifier currently holds on (low battery, storage, network down, version
/// mismatch), as configured on the notification settings. Shared with
/// `page_stats_ctx` for the same reason [`hub_value_class`] is.
pub fn hub_fault(device: &DeviceState, faults: &crate::notify::HubFaults) -> bool {
    !device.online || faults.any(&device.hostname) || dc_missing(device)
}

/// An online Hub that reports its DC (PoE/adapter) input off: running on the cell, critical (#181).
pub fn dc_missing(device: &DeviceState) -> bool {
    device.online && device.power.dc_connected == Some(false)
}

/// The fault triangle's tooltip for [`hub_fault`], as i18n keys: the offline
/// sentence when the Hub is offline, then the event-type name of every other
/// condition on (`alarms.event_type.*`, the names the notification history
/// uses). The notifier's own offline condition is the first one again.
pub fn hub_fault_keys(device: &DeviceState, faults: &crate::notify::HubFaults) -> Vec<String> {
    use crate::notify::SystemAlertKind;
    let mut keys = Vec::new();
    if !device.online {
        keys.push("monitoring.hubs.fault_offline".to_string());
    }
    if dc_missing(device) {
        keys.push("monitoring.device.no_power_badge".to_string());
    }
    for kind in faults.kinds(&device.hostname) {
        if kind == SystemAlertKind::DeviceOffline {
            if device.online {
                // The notifier has not seen the Hub come back yet; the store has.
                continue;
            }
            if keys.is_empty() {
                keys.push("monitoring.hubs.fault_offline".to_string());
            }
            continue;
        }
        if kind == SystemAlertKind::BatteryLow
            && device.power.battery_percentage.is_some_and(|p| p < 10)
        {
            // The notifier has one battery condition; below 10 % say it is critical (#181).
            keys.push("monitoring.device.critical_battery_badge".to_string());
            continue;
        }
        keys.push(format!("alarms.event_type.{}", kind.as_str()));
    }
    keys
}

pub fn hub_row_ctx(device: &DeviceState, faults: &crate::notify::HubFaults) -> HubRowCtx {
    HubRowCtx {
        battery: device.power.battery_percentage.map(|p| format!("{p} %")),
        battery_tone: match device.power.battery_percentage {
            Some(p) if p < 10 => "critical",
            Some(p) if p < 20 => "warning",
            _ => "",
        },
        hostname: device.hostname.clone(),
        location: None,
        description: None,
        name: device
            .system
            .device_label
            .clone()
            .or_else(|| device.config.device_label.clone()),
        serial: hub_serial(device),
        online: device.online,
        last_seen: device.last_seen.and_then(unix_to_rfc3339),
        display_version: display_version(device.version.as_deref()),
        gateway_eui: device.lorawan.as_ref().and_then(|l| l.gateway_eui.clone()),
        // The card's presence from the diagnostics document, not v1's stale flag (#181).
        radio: device.lorawan.as_ref().map(|l| HubRadioCtx {
            gateway_present: rak_card_present(device).or(l.gateway_present),
            ..HubRadioCtx::from_info(l)
        }),
        radio_control_off: false,
        unpaired: false,
        lorawan_disabled: lorawan_disabled(device),
        northbridge: northbridge_ctx(device),
        value_class: hub_value_class(device),
        fault: hub_fault(device, faults),
        fault_keys: hub_fault_keys(device, faults),
        fault_critical: dc_missing(device)
            || device.power.battery_percentage.is_some_and(|p| p < 10),
        kind: None,
        cluster: None,
        calibration: None,
        ip: device
            .network
            .ethernet_ip
            .clone()
            .or_else(|| device.network.wifi_ip.clone()),
        fields: hub_row_fields(device),
    }
}

fn hub_serial(device: &DeviceState) -> Option<String> {
    device
        .system
        .hardware
        .as_ref()
        .and_then(|h| h.serial.clone())
}

/// The Overview "Device description" card (devices.hub.md §3.1, amendment
/// 2026-09-28): serial, model / revision and radio mode, each straight from
/// `system/info` and `None` — "—" on the page — when the Hub has not
/// reported it. No gateway EUI: it lives in the card's "Brány" section.
#[derive(Debug, Clone, Serialize)]
pub struct HubDescriptionCtx {
    /// The full CPU serial, `hardware.serial`.
    pub serial: Option<String>,
    /// The "Model / revision" row as one value: the compute module's model,
    /// then " / " and its revision when the Hub reported one.
    pub model_revision: Option<String>,
    /// `lorawan.mode` when it is one of the four this build labels
    /// (`monitoring.hubs.radio_mode_*`, the System tab's labels).
    pub radio_mode: Option<&'static str>,
    /// `lorawan.mode` verbatim when it is not: a newer agent's mode is shown
    /// as the Hub spells it rather than as a raw i18n key.
    pub radio_mode_other: Option<String>,
}

pub fn hub_description_ctx(device: &DeviceState) -> HubDescriptionCtx {
    let hardware = device.system.hardware.as_ref();
    let model_revision = hardware.and_then(|h| match (h.model.as_deref(), h.revision.as_deref()) {
        (Some(model), Some(revision)) => Some(format!("{model} / {revision}")),
        (Some(model), None) => Some(model.to_string()),
        (None, Some(revision)) => Some(format!("— / {revision}")),
        (None, None) => None,
    });
    let mode = device.lorawan.as_ref().and_then(|l| l.mode.as_deref());
    let known = mode.and_then(|m| {
        control_radio::Mode::ALL
            .into_iter()
            .find(|k| k.as_str() == m)
            .map(|k| k.as_str())
    });
    HubDescriptionCtx {
        serial: hub_serial(device),
        model_revision,
        radio_mode: known,
        radio_mode_other: mode.filter(|_| known.is_none()).map(str::to_string),
    }
}

/// The Hub's name as every surface shows it (devices.hub.md §3.1): the
/// operator's own name (`device_meta.name`, V16) first, then the
/// `device_label` the unit reports, then `None` — and `None` is where the
/// caller falls back to the hostname, which is the only name that is always
/// there.
///
/// The operator's name wins because it is the one a factory reset cannot
/// take away: `device_label` lives in the device config and goes with it.
pub fn hub_display_name(
    device: Option<&DeviceState>,
    meta: Option<&portal_storage::DeviceMeta>,
) -> Option<String> {
    meta.and_then(|m| m.name.clone()).or_else(|| {
        device.and_then(|d| {
            d.system
                .device_label
                .clone()
                .or_else(|| d.config.device_label.clone())
        })
    })
}

/// [`hub_row_ctx`] with the operator's name applied over `device_label`.
///
/// Separate from `hub_row_ctx` rather than a parameter on it because the
/// row's every other field comes from the store alone, while this one needs
/// a database read the list page does per row (§2.7: units of rows).
pub fn hub_row_ctx_with_meta(
    device: &DeviceState,
    meta: Option<&portal_storage::DeviceMeta>,
    faults: &crate::notify::HubFaults,
) -> HubRowCtx {
    HubRowCtx {
        name: hub_display_name(Some(device), meta),
        location: meta.and_then(|m| m.location.clone()),
        description: meta.and_then(|m| m.description.clone()),
        ..hub_row_ctx(device, faults)
    }
}

/// The `system/info.lorawan` health block as the Hub pages show it: the
/// service chips and the UI-4 "gateway: waiting / no radio" state, which is
/// a degraded-green fact about the unit, never an error (devices.hub.md §3.1).
#[derive(Debug, Clone, Serialize)]
pub struct HubRadioCtx {
    /// `false` = no concentrator attached; the chips then say nothing about
    /// services that have no radio to run. `None` = the Hub did not say.
    pub gateway_present: Option<bool>,
    pub concentratord_running: Option<bool>,
    pub chirpstack_running: Option<bool>,
    pub gateway_eui: Option<String>,
    pub sensor_count: Option<i64>,
}

impl HubRadioCtx {
    fn from_info(info: &fiber_protocol::payloads::system_info::LorawanGatewayInfo) -> Self {
        HubRadioCtx {
            gateway_present: info.gateway_present,
            concentratord_running: info.concentratord_running,
            chirpstack_running: info.chirpstack_running,
            gateway_eui: info.gateway_eui.clone(),
            sensor_count: info.sensor_count,
        }
    }
}

/// One row of the "Radio & gateways" table (devices.hub.md §3.1).
#[derive(Debug, Clone, Serialize)]
pub struct GatewayRowCtx {
    pub gateway_eui: String,
    pub name: String,
    /// The Hub's own concentrator: its EUI equals `system/info.lorawan.gateway_eui`.
    pub local: bool,
    /// The cluster Client whose concentrator this is, on the Server's card.
    pub client: Option<String>,
    /// Recomputed on the producer's 300 s window at render time; the report's
    /// own `status` string ages between the 60 s publishes.
    pub online: bool,
    /// RFC 3339, for `short_dt`.
    pub last_seen: Option<String>,
    pub rssi: Option<i32>,
    pub snr: Option<f64>,
}

/// `partials/hub_radio_card.html`'s context, from the two sources §12 A/A2
/// bring into the store. Either half can be missing on its own: a Hub that
/// has sent `system/info` but whose `radio/gateways` report never arrived
/// still shows its service chips, and the card says the gateway table is
/// unavailable instead of pretending every gateway is offline (§2.4, UI-2).
#[derive(Debug, Clone, Serialize)]
pub struct RadioCardCtx {
    pub hostname: String,
    pub radio: Option<HubRadioCtx>,
    /// Same fact as [`HubRowCtx::radio_control_off`]; the route sets it.
    pub radio_control_off: bool,
    /// `None` until a report has been received (or after the producer
    /// withdrew it with the empty retained payload).
    pub gateways: Option<GatewaysCardCtx>,
    /// The northbridge row (radio diagnostics §4); `None` when the Hub publishes no
    /// document or the document says `disabled` — the row is hidden, not greyed.
    pub northbridge: Option<NorthbridgeCardCtx>,
    /// The System tab's Northbridge firmware card (#158): [`Self::northbridge`], plus the
    /// `disabled` document of a Hub without P2P whose idle Northbridge task reports the
    /// firmware and takes an update.
    pub nb_firmware: Option<NorthbridgeCardCtx>,
    /// The LoRaWAN block (§4a); `None` falls back to the `system/info.lorawan` chips.
    pub lorawan: Option<LorawanDiagCtx>,
    /// The `radio/lorawan` document says `disabled` (`radio.mode` `p2p` or `off`): ChirpStack
    /// and the concentrator are off by design, so the card shows one neutral line instead of
    /// the chips, the API banner and the gateway table, which would all read as faults (PN-2).
    pub lorawan_disabled: bool,
    /// The viewer may ask for „Aktualizovat firmware NB“ (admin; the route sets it). The button
    /// shows only where it can do something: a Northbridge row with an `update` object (the
    /// image bundles firmware) that is not `flashing`, and no update this page follows.
    pub nb_update_allowed: bool,
    /// The Hub is online: the firmware cards' Update button is disabled, with that reason,
    /// while it is not.
    pub online: bool,
    /// A manual Northbridge update this page follows ([`nb_update_progress`]); the route sets
    /// it from the card's own query.
    pub nb_progress: Option<NbUpdateProgressCtx>,
    /// The Hub is a cluster Client: it runs no ChirpStack (control/cluster.md §2, its
    /// forwarder feeds the Server's), so the card draws no chirpstack chip or row. The
    /// route sets it from the registry and the Hub's reported role.
    pub client: bool,
}

/// How long after a manual Northbridge update was asked for the card waits for the Hub to
/// report it `flashing` before it reads the document as the outcome: the P2P task looks for a
/// request every 2 s and publishes a changed `update` within a second or two.
pub const NB_UPDATE_START_GRACE_S: i64 = 90;
/// How long the card keeps following an update at all, its outcome line included.
pub const NB_UPDATE_FOLLOW_S: i64 = 600;

/// Where a manual Northbridge update the card follows has got to — the `nbu` query of the
/// card's own refetch, carried forward so the polling card knows whether it has seen the Hub
/// start (`plan/control/radio/northbridge_update.md` §3.4, amended 2026-10-09).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NbUpdatePhase {
    /// Asked for; the Hub has not reported `flashing` yet.
    Wait,
    /// The Hub reported `flashing`.
    Run,
    /// Over; the card shows the outcome.
    Done,
}

impl NbUpdatePhase {
    pub fn parse(text: &str) -> Option<NbUpdatePhase> {
        match text {
            "wait" => Some(NbUpdatePhase::Wait),
            "run" => Some(NbUpdatePhase::Run),
            "done" => Some(NbUpdatePhase::Done),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            NbUpdatePhase::Wait => "wait",
            NbUpdatePhase::Run => "run",
            NbUpdatePhase::Done => "done",
        }
    }
}

/// The card's line about a manual Northbridge update it follows.
#[derive(Debug, Clone, Serialize)]
pub struct NbUpdateProgressCtx {
    /// `running` (asked for or flashing) | `ok` | `failed` | `unconfirmed`.
    pub outcome: &'static str,
    /// The version the Northbridge runs (`ok`, `unconfirmed`), or the Hub's error (`failed`).
    pub detail: Option<String>,
    /// The card refetches itself every few seconds until the outcome is known.
    pub poll: bool,
    /// The query the card refetches itself with, `?nbu=<phase>&since=<unix>`.
    pub query: String,
}

/// The progress of a manual Northbridge update asked for at `since` (unix seconds) and last
/// seen in `phase`, read off the current Northbridge row; `None` once it is older than
/// [`NB_UPDATE_FOLLOW_S`] (or from the future).
///
/// `flashing` is the Hub at work, whatever the phase. Otherwise a `wait` within
/// [`NB_UPDATE_START_GRACE_S`] is still the Hub picking the request up; past it, and after a
/// `run`, the document is the outcome: `failed` state or a `last_error` is a failure with the
/// Hub's words, the running image equal to the bundled one a success, anything else (a request
/// the Hub refused without flashing, a missing row) unconfirmed — the firmware line and the
/// Hub's note say the rest.
pub fn nb_update_progress(
    nb: Option<&NorthbridgeCardCtx>,
    phase: NbUpdatePhase,
    since: i64,
    now: i64,
) -> Option<NbUpdateProgressCtx> {
    let age = now - since;
    if !(0..=NB_UPDATE_FOLLOW_S).contains(&age) {
        return None;
    }
    let update = nb.and_then(|nb| nb.update.as_ref());
    let state = update.map(|u| u.state.as_str());
    let at = |phase: NbUpdatePhase| format!("?nbu={}&since={since}", phase.as_str());
    if state == Some("flashing") {
        return Some(NbUpdateProgressCtx {
            outcome: "running",
            detail: None,
            poll: true,
            query: at(NbUpdatePhase::Run),
        });
    }
    if phase == NbUpdatePhase::Wait && age < NB_UPDATE_START_GRACE_S {
        return Some(NbUpdateProgressCtx {
            outcome: "running",
            detail: None,
            poll: true,
            query: at(NbUpdatePhase::Wait),
        });
    }
    let (outcome, detail) = match update {
        Some(u) if u.state == "failed" || u.last_error.is_some() => {
            ("failed", u.last_error.clone().or_else(|| u.note.clone()))
        }
        Some(u) if u.verdict == "current" => ("ok", u.running.clone()),
        Some(u) => ("unconfirmed", u.running.clone()),
        None => ("unconfirmed", None),
    };
    Some(NbUpdateProgressCtx {
        outcome,
        detail,
        poll: false,
        query: at(NbUpdatePhase::Done),
    })
}

/// The System tab's "Firmware SB" card (`plan/system/southbridge_update.md` §3.7;
/// devices.hub.md, dodatek 2026-10-09): `system/info.southbridge` as the Hub reported it, and
/// „Aktualizovat firmware SB“ for an admin. Refetched on its own
/// (`GET /hubs/{hub}/sb-firmware`), so the confirm dialog lives outside it.
#[derive(Debug, Clone, Serialize)]
pub struct SbFirmwareCtx {
    pub hostname: String,
    /// `None` when the Hub has not reported the block (an older agent): the card says "—".
    pub sb: Option<SouthbridgeCtx>,
    /// The viewer may ask for the update (admin; the route sets it).
    pub update_allowed: bool,
    /// The button shows: admin, an `update` object, a bundled image, not `running`, and no
    /// update this card is still polling for.
    pub offer_update: bool,
    /// The Hub is online (the button's disabled reason otherwise).
    pub online: bool,
    /// A manual update this card follows ([`sb_update_progress`]), from its own query.
    pub progress: Option<NbUpdateProgressCtx>,
}

/// `system/info.southbridge`, flattened for the card.
#[derive(Debug, Clone, Serialize)]
pub struct SouthbridgeCtx {
    /// What the SB answers to `VERSION`, e.g. "south-zephyr 1.1.0 g73f39ed5".
    pub version: Option<String>,
    /// The same for the image this Hub bundles.
    pub bundled: Option<String>,
    /// Whether the block carries an `update` object at all.
    pub has_update: bool,
    /// `idle` | `running` | `failed`, or what a newer agent says.
    pub state: Option<String>,
    pub attempts: Option<u32>,
    pub last_error: Option<String>,
    /// `ok <version>` or `failed`.
    pub last_result: Option<String>,
}

impl SouthbridgeCtx {
    pub fn from_info(sb: &fiber_protocol::payloads::system_info::Southbridge) -> SouthbridgeCtx {
        let update = sb.update.as_ref();
        SouthbridgeCtx {
            version: sb.version.clone(),
            bundled: sb.bundled.clone(),
            has_update: update.is_some(),
            state: update.and_then(|u| u.state.clone()),
            attempts: update.and_then(|u| u.attempts),
            last_error: update.and_then(|u| u.last_error.clone()),
            last_result: update.and_then(|u| u.last_result.clone()),
        }
    }

    pub fn running(&self) -> bool {
        self.state.as_deref() == Some("running")
    }

    /// Whether „Aktualizovat firmware SB“ can do anything here: the Hub reports an update
    /// object and a bundled image, and no update is running.
    pub fn can_update(&self) -> bool {
        self.has_update && self.bundled.is_some() && !self.running()
    }
}

/// How long after a manual Southbridge update was asked for the card waits for the Hub to
/// report it `running` before it reads the block as the outcome. The Hub marks it `running`
/// before it answers and republishes `system/info` at once, so this only covers a slow broker.
pub const SB_UPDATE_START_GRACE_S: i64 = 60;
/// How long the card keeps following an update at all, its outcome line included.
pub const SB_UPDATE_FOLLOW_S: i64 = 600;

/// The progress of a manual Southbridge update asked for at `since` and last seen in `phase`
/// (the `sbu` query; the phases are the Northbridge's, [`NbUpdatePhase`]); `None` once older
/// than [`SB_UPDATE_FOLLOW_S`] or from the future.
///
/// `running` is the Hub at work. Otherwise a `wait` within [`SB_UPDATE_START_GRACE_S`] is the
/// Hub picking the request up; past it, and after a `run`, the block is the outcome: `failed`
/// with the Hub's `last_error`, `last_result: ok …` a success with the version the SB now
/// answers, anything else (no block, no result) unconfirmed.
pub fn sb_update_progress(
    sb: Option<&SouthbridgeCtx>,
    phase: NbUpdatePhase,
    since: i64,
    now: i64,
) -> Option<NbUpdateProgressCtx> {
    let age = now - since;
    if !(0..=SB_UPDATE_FOLLOW_S).contains(&age) {
        return None;
    }
    let at = |phase: NbUpdatePhase| format!("?sbu={}&since={since}", phase.as_str());
    let running = |phase| NbUpdateProgressCtx {
        outcome: "running",
        detail: None,
        poll: true,
        query: at(phase),
    };
    if sb.is_some_and(SouthbridgeCtx::running) {
        return Some(running(NbUpdatePhase::Run));
    }
    if phase == NbUpdatePhase::Wait && age < SB_UPDATE_START_GRACE_S {
        return Some(running(NbUpdatePhase::Wait));
    }
    let (outcome, detail) = match sb {
        Some(s) if s.state.as_deref() == Some("failed") => (
            "failed",
            s.last_error.clone().or_else(|| s.last_result.clone()),
        ),
        Some(s)
            if s.last_result
                .as_deref()
                .is_some_and(|r| r.starts_with("ok")) =>
        {
            (
                "ok",
                s.version.clone().or_else(|| {
                    s.last_result
                        .as_deref()
                        .and_then(|r| r.strip_prefix("ok "))
                        .map(str::to_string)
                }),
            )
        }
        Some(s) => ("unconfirmed", s.version.clone()),
        None => ("unconfirmed", None),
    };
    Some(NbUpdateProgressCtx {
        outcome,
        detail,
        poll: false,
        query: at(NbUpdatePhase::Done),
    })
}

/// The "Firmware SB" card for `device`; `admin` and a followed update come from the route.
pub fn sb_firmware_ctx(
    device: &DeviceState,
    admin: bool,
    progress: Option<(NbUpdatePhase, i64)>,
    now: i64,
) -> SbFirmwareCtx {
    let sb = device
        .system
        .southbridge
        .as_ref()
        .map(SouthbridgeCtx::from_info);
    let progress =
        progress.and_then(|(phase, since)| sb_update_progress(sb.as_ref(), phase, since, now));
    let polling = progress.as_ref().is_some_and(|p| p.poll);
    SbFirmwareCtx {
        hostname: device.hostname.clone(),
        offer_update: admin && !polling && sb.as_ref().is_some_and(SouthbridgeCtx::can_update),
        sb,
        update_allowed: admin,
        online: device.online,
        progress,
    }
}

/// A radio diagnostics state as the card colours it (§4): `alive` green, `degraded`
/// amber, `error`/`unreachable` red, anything else (`starting`, unknown) neutral.
fn diag_badge(state: &str) -> &'static str {
    match state {
        "alive" => "normal",
        "degraded" => "warning",
        "error" | "unreachable" => "critical",
        _ => "neutral",
    }
}

/// The last error of a radio document: `{at, message}` plus the northbridge's `code`.
#[derive(Debug, Clone, Serialize)]
pub struct DiagErrorCtx {
    /// RFC 3339, for `short_dt`.
    pub at: Option<String>,
    pub message: String,
    pub code: Option<u64>,
}

fn diag_error(doc: &serde_json::Value) -> Option<DiagErrorCtx> {
    let e = doc.get("last_error")?.as_object()?;
    Some(DiagErrorCtx {
        at: e.get("at").and_then(|v| v.as_str()).map(str::to_string),
        message: e
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        code: e.get("code").and_then(|v| v.as_u64()),
    })
}

fn str_at(doc: &serde_json::Value, path: &[&str]) -> Option<String> {
    path.iter()
        .try_fold(doc, |v, k| v.get(k))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn u64_at(doc: &serde_json::Value, path: &[&str]) -> Option<u64> {
    path.iter()
        .try_fold(doc, |v, k| v.get(k))
        .and_then(|v| v.as_u64())
}

/// The northbridge row of the radio card (radio diagnostics §4).
///
/// Two document shapes reach it: the 0.2.x modem's (`proto_version` 1, seven counters,
/// `radio.state` `idle`/`rx`/`tx`/`error`) and the TOWER gateway's (`p2p_tower_gateway.md`
/// §2: `git_sha`, `build_flags`, `gateway{…}`, 25 counters, `radio.state`
/// `boot`/`configured`/`gateway`, no `proto_version`). An old Hub keeps its old card.
#[derive(Debug, Clone, Serialize)]
pub struct NorthbridgeCardCtx {
    /// `starting` | `alive` | `error` | `unreachable`.
    pub state: String,
    /// The i18n key for `state`, `None` for a state this build does not label (shown
    /// verbatim, never as a key).
    pub state_key: Option<&'static str>,
    pub badge: &'static str,
    pub fw_version: Option<String>,
    /// `None` for `0` or absent: the TOWER document dropped it (flag day).
    pub proto_version: Option<u64>,
    /// The image's commit, short hex (TOWER).
    pub git_sha: Option<String>,
    /// `bench_rtt_bridge`, `debug`, … (TOWER); empty for a release image.
    pub build_flags: Vec<String>,
    /// `"3 d 4 h"`, from `uptime_s`.
    pub uptime: Option<String>,
    /// Decoded reset causes of the current boot (`pin`, `watchdog`, …).
    pub reset_cause: Vec<String>,
    /// `watchdog` or `brownout` among them — worth a warning on its own (§3).
    pub reset_warning: bool,
    /// RFC 3339.
    pub boot_at: Option<String>,
    /// `idle` | `rx` | `tx` | `error` (0.2.x), `boot` | `configured` | `gateway` (TOWER).
    pub radio_state: Option<String>,
    /// The i18n key for a TOWER `radio_state`; `None` shows `radio_state` verbatim.
    pub radio_state_key: Option<&'static str>,
    pub freq_hz: Option<u64>,
    pub sf: Option<u64>,
    pub tx_power_dbm: Option<i64>,
    /// The TOWER gateway's own state, reduced to what an operator reads.
    pub gateway: Option<NorthbridgeGatewayCtx>,
    /// `(name, value)` behind the disclosure: the document's order for 0.2.x, a fixed
    /// subset of the 25 for TOWER ([`TOWER_COUNTERS`]), none before its first `GET_INFO`.
    pub counters: Vec<(String, u64)>,
    pub err_flags: Vec<String>,
    pub last_error: Option<DiagErrorCtx>,
    /// The firmware update (`northbridge_update.md` §3.5); `None` when the Hub bundles no
    /// Northbridge firmware or predates U4.
    pub update: Option<NorthbridgeUpdateCtx>,
    /// When the portal received the document, RFC 3339.
    pub received_at: Option<String>,
}

/// The firmware line of the northbridge row: what runs against what the Hub image bundles,
/// and how the last update went (`northbridge_update.md` §3.4–§3.5).
#[derive(Debug, Clone, Serialize)]
pub struct NorthbridgeUpdateCtx {
    /// `idle` | `flashing` | `failed`, or a state this build does not know.
    pub state: String,
    /// `current` | `differs` | `flashing` | `failed` | `unknown` — the i18n key suffix.
    pub verdict: &'static str,
    pub badge: &'static str,
    /// `x.y.z+sha8` the Hub image bundles.
    pub bundled: Option<String>,
    /// `x.y.z+sha8` the Northbridge reports, when it does.
    pub running: Option<String>,
    pub attempts: u64,
    pub last_error: Option<String>,
    /// The Hub's own note: a one-time J-Link migration is needed, a hotfix is kept, …
    pub note: Option<String>,
}

impl NorthbridgeUpdateCtx {
    fn from_doc(doc: &serde_json::Value) -> Option<Self> {
        let u = doc.get("update").filter(|u| u.is_object())?;
        let state = str_at(u, &["state"]).unwrap_or_default();
        let bundled = str_at(u, &["bundled"]);
        let running = str_at(doc, &["fw_version"]).map(|fw| {
            match str_at(doc, &["git_sha"]).filter(|s| !s.is_empty()) {
                Some(sha) => format!("{fw}+{}", sha.chars().take(8).collect::<String>()),
                None => fw,
            }
        });
        let verdict = match state.as_str() {
            "failed" => "failed",
            "flashing" => "flashing",
            _ => match (&running, &bundled) {
                (Some(r), Some(b)) if r == b => "current",
                (Some(_), Some(_)) => "differs",
                _ => "unknown",
            },
        };
        let badge = match verdict {
            "current" => "normal",
            "differs" => "warning",
            "failed" => "warning",
            _ => "neutral",
        };
        Some(NorthbridgeUpdateCtx {
            verdict,
            badge,
            bundled,
            running,
            attempts: u64_at(u, &["attempts"]).unwrap_or(0),
            last_error: str_at(u, &["last_error"]),
            note: str_at(u, &["note"]),
            state,
        })
    }
}

/// The TOWER `gateway{…}` object as the card shows it.
#[derive(Debug, Clone, Serialize)]
pub struct NorthbridgeGatewayCtx {
    /// `None` before the first `TWR_START` (`0`). Decimal, as the Node detail shows a P2P
    /// `net_id`.
    pub net_id: Option<u64>,
    pub peers: Option<u64>,
    pub peers_max: Option<u64>,
    pub queue_used: Option<u64>,
    pub queue_max: Option<u64>,
}

/// The TOWER counters the card shows, in this order: the seven the 0.2.x document had, then
/// the two that say most about Nodes the gateway hears but cannot use (link doc §3.1 names).
/// The other 17 stay in the document for `proximosctl`.
const TOWER_COUNTERS: [&str; 9] = [
    "rx",
    "tx",
    "crc_err",
    "link_err",
    "rx_rearm_fail",
    "tx_radio_err",
    "tx_refused",
    "rx_mic_fail",
    "rx_dup",
];

/// The i18n key of a northbridge state — the document's own `state` or the TOWER
/// `radio.state` — or `None` for one this build does not label.
fn nb_state_key(state: &str) -> Option<&'static str> {
    Some(match state {
        "starting" => "monitoring.hubs.radio_state.starting",
        "alive" => "monitoring.hubs.radio_state.alive",
        "degraded" => "monitoring.hubs.radio_state.degraded",
        "error" => "monitoring.hubs.radio_state.error",
        "unreachable" => "monitoring.hubs.radio_state.unreachable",
        _ => return None,
    })
}

/// The i18n key of a TOWER `radio.state`; the 0.2.x values stay verbatim, as before.
fn nb_radio_state_key(state: &str) -> Option<&'static str> {
    Some(match state {
        "boot" => "monitoring.hubs.radio_nb_mode.boot",
        "configured" => "monitoring.hubs.radio_nb_mode.configured",
        "gateway" => "monitoring.hubs.radio_nb_mode.gateway",
        _ => return None,
    })
}

fn str_list(doc: &serde_json::Value, key: &str) -> Vec<String> {
    doc.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

impl NorthbridgeCardCtx {
    fn from_doc(doc: &serde_json::Value, received_at: i64) -> Option<Self> {
        if doc.get("state")?.as_str()? == "disabled" {
            return None;
        }
        Self::from_any_doc(doc, received_at)
    }

    /// For the firmware card only (#158): also a `disabled` document from the idle
    /// Northbridge task (`nb_task: "idle"`), which reports the firmware and answers an update
    /// on a Hub without P2P. An older Hub's `disabled` document claims nothing and stays `None`.
    fn from_firmware_doc(doc: &serde_json::Value, received_at: i64) -> Option<Self> {
        let idle = doc.get("nb_task").and_then(|v| v.as_str()) == Some("idle");
        if doc.get("state")?.as_str()? == "disabled" && !idle {
            return None;
        }
        Self::from_any_doc(doc, received_at)
    }

    fn from_any_doc(doc: &serde_json::Value, received_at: i64) -> Option<Self> {
        let state = doc.get("state")?.as_str()?.to_string();
        // The TOWER central publishes `proto_version` 0 (flag day) and the gateway's own
        // counters, from `starting` on; the 0.2.x document had `proto_version` 1.
        let tower = u64_at(doc, &["proto_version"]) == Some(0)
            || doc.get("git_sha").is_some()
            || doc.get("gateway").is_some()
            || doc.pointer("/counters/rx_fresh").is_some();
        // Before the first `GET_INFO` the TOWER counters are placeholders at 0, not readings.
        let has_info = !tower || doc.get("fw_version").is_some_and(|v| !v.is_null());
        let reset_cause: Vec<String> = doc
            .pointer("/boot/reset_cause")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let all_counters: Vec<(String, u64)> = doc
            .get("counters")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n)))
                    .collect()
            })
            .unwrap_or_default();
        let counters = if !has_info {
            Vec::new()
        } else if tower {
            TOWER_COUNTERS
                .iter()
                .filter_map(|name| all_counters.iter().find(|(k, _)| k == name))
                .cloned()
                .collect()
        } else {
            all_counters
        };
        let gateway = doc
            .get("gateway")
            .filter(|g| g.is_object())
            .map(|g| NorthbridgeGatewayCtx {
                net_id: u64_at(g, &["net_id"]).filter(|&n| n != 0),
                peers: u64_at(g, &["peers"]),
                peers_max: u64_at(g, &["peers_max"]),
                queue_used: u64_at(g, &["queue_used"]),
                queue_max: u64_at(g, &["queue_max"]),
            });
        let radio_state = str_at(doc, &["radio", "state"]);
        Some(NorthbridgeCardCtx {
            state_key: nb_state_key(&state),
            badge: diag_badge(&state),
            fw_version: str_at(doc, &["fw_version"]),
            proto_version: u64_at(doc, &["proto_version"]).filter(|&v| v != 0),
            git_sha: str_at(doc, &["git_sha"])
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().take(8).collect()),
            build_flags: str_list(doc, "build_flags"),
            uptime: u64_at(doc, &["uptime_s"])
                .map(|s| super::alarm::humanize_duration_secs(s as i64)),
            reset_warning: reset_cause
                .iter()
                .any(|c| c == "watchdog" || c == "brownout"),
            reset_cause,
            boot_at: str_at(doc, &["boot", "at"]),
            radio_state_key: radio_state.as_deref().and_then(nb_radio_state_key),
            radio_state,
            freq_hz: u64_at(doc, &["radio", "freq_hz"]),
            sf: u64_at(doc, &["radio", "sf"]),
            tx_power_dbm: doc.pointer("/radio/tx_power_dbm").and_then(|v| v.as_i64()),
            gateway,
            counters,
            err_flags: str_list(doc, "err_flags"),
            last_error: diag_error(doc),
            update: NorthbridgeUpdateCtx::from_doc(doc),
            received_at: unix_to_rfc3339(received_at),
            state,
        })
    }
}

/// One component row of the LoRaWAN block (§4a): its unit state and the one detail that
/// matters for it.
#[derive(Debug, Clone, Serialize)]
pub struct LorawanRowCtx {
    /// `card` | `concentratord` | `forwarder` | `chirpstack` — the i18n key suffix.
    pub key: &'static str,
    pub badge: &'static str,
    /// `active`, `failed (exit-code)`, `present`, `missing`, `unknown`.
    pub status: String,
    /// Version, device, API status, stats age — whatever the row has, already joined.
    pub details: Vec<String>,
}

/// The LoRaWAN block of the radio card (§4a).
#[derive(Debug, Clone, Serialize)]
pub struct LorawanDiagCtx {
    /// `starting` | `alive` | `degraded` | `error`.
    pub state: String,
    pub badge: &'static str,
    pub rows: Vec<LorawanRowCtx>,
    pub gateway_eui: Option<String>,
    /// RFC 3339: the concentrator's last stats frame.
    pub last_stats: Option<String>,
    /// `(name, value)` of the last stats frame.
    pub stats: Vec<(String, u64)>,
    pub last_error: Option<DiagErrorCtx>,
    pub received_at: Option<String>,
}

impl LorawanDiagCtx {
    fn from_doc(doc: &serde_json::Value, received_at: i64) -> Option<Self> {
        let state = doc.get("state")?.as_str()?.to_string();
        if state == "disabled" {
            return None;
        }
        let unit_row = |key: &'static str, comp: &str, extra: Vec<String>| {
            let unit = str_at(doc, &[comp, "unit"]).unwrap_or_else(|| "unknown".into());
            let restarts = u64_at(doc, &[comp, "restarts"]).unwrap_or(0);
            let mut status = match str_at(doc, &[comp, "result"]) {
                Some(r) if unit != "active" && r != "success" => format!("{unit} ({r})"),
                _ => unit.clone(),
            };
            if restarts > 0 {
                status.push_str(&format!(" · ↻{restarts}"));
            }
            LorawanRowCtx {
                key,
                badge: match unit.as_str() {
                    "active" => "normal",
                    "activating" | "reloading" | "unknown" => "neutral",
                    _ => "critical",
                },
                status,
                details: extra,
            }
        };

        let present = doc.pointer("/card/present").and_then(|v| v.as_bool());
        let card = LorawanRowCtx {
            key: "card",
            badge: match present {
                Some(true) => "normal",
                Some(false) => "critical",
                None => "neutral",
            },
            status: match present {
                Some(true) => "present".into(),
                Some(false) => "missing".into(),
                None => "unknown".into(),
            },
            details: [
                str_at(doc, &["card", "device"]),
                str_at(doc, &["card", "usb_id"]),
            ]
            .into_iter()
            .flatten()
            .collect(),
        };
        let concentratord = unit_row(
            "concentratord",
            "concentratord",
            str_at(doc, &["concentratord", "version"])
                .into_iter()
                .collect(),
        );
        let mut forwarder = unit_row("forwarder", "forwarder", Vec::new());
        match str_at(doc, &["forwarder", "conn"]).as_deref() {
            Some("offline") => {
                forwarder.details.push("OFFLINE".into());
                if forwarder.badge == "normal" {
                    forwarder.badge = "warning";
                }
            }
            Some(c) => forwarder.details.push(c.to_ascii_uppercase()),
            None => {}
        }
        let mut chirpstack = unit_row(
            "chirpstack",
            "chirpstack",
            str_at(doc, &["chirpstack", "version"])
                .into_iter()
                .collect(),
        );
        if let Some(api) = str_at(doc, &["chirpstack", "api"]) {
            if api != "ok" && chirpstack.badge == "normal" {
                chirpstack.badge = if api == "auth_failed" {
                    "critical"
                } else {
                    "warning"
                };
            }
            chirpstack.details.push(format!("API {api}"));
        }

        Some(LorawanDiagCtx {
            badge: diag_badge(&state),
            rows: vec![card, concentratord, forwarder, chirpstack],
            gateway_eui: str_at(doc, &["concentratord", "gateway_eui"]),
            last_stats: str_at(doc, &["concentratord", "last_stats"]),
            stats: doc
                .pointer("/concentratord/stats")
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n)))
                        .collect()
                })
                .unwrap_or_default(),
            last_error: diag_error(doc),
            received_at: unix_to_rfc3339(received_at),
            state,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GatewaysCardCtx {
    /// `"ok"` | `"auth_failed"` | `"unreachable"`; anything but `ok` renders
    /// the banner while the rows keep their last known state.
    pub api_status: &'static str,
    /// When the report reached the portal, RFC 3339.
    pub received_at: Option<String>,
    pub rows: Vec<GatewayRowCtx>,
}

/// UI-26: the card lists only gateways that belong to this Hub. Foreign
/// gateways live on `/external`. "Belongs" means the Hub's own EUI or, on a
/// Server, the gateway of one of its cluster Clients (`clients`: EUI,
/// lowercase, and the Client's hostname), which the Server's ChirpStack serves.
pub fn radio_card_ctx(
    device: &DeviceState,
    gateways: Option<&RadioGatewaysState>,
    clients: &[(String, String)],
) -> RadioCardCtx {
    let local_eui = device
        .lorawan
        .as_ref()
        .and_then(|l| l.gateway_eui.as_deref());
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let gateways = gateways.map(|state| GatewaysCardCtx {
        api_status: match state.report.api_status {
            crate::state::ApiStatus::Ok => "ok",
            crate::state::ApiStatus::AuthFailed => "auth_failed",
            crate::state::ApiStatus::Unreachable => "unreachable",
        },
        received_at: unix_to_rfc3339(state.received_at),
        rows: state
            .report
            .gateways
            .iter()
            .filter_map(|g| {
                let local = local_eui.is_some_and(|l| l.eq_ignore_ascii_case(&g.gateway_eui));
                let client = clients
                    .iter()
                    .find(|(eui, _)| eui.eq_ignore_ascii_case(&g.gateway_eui))
                    .map(|(_, host)| host.clone());
                (local || client.is_some()).then_some((g, local, client))
            })
            .map(|(g, local, client)| GatewayRowCtx {
                gateway_eui: g.gateway_eui.clone(),
                name: g.name.clone(),
                local,
                client,
                online: g.is_online(now),
                last_seen: g.last_seen.clone(),
                rssi: g.rssi,
                snr: g.snr,
            })
            .collect(),
    });
    RadioCardCtx {
        hostname: device.hostname.clone(),
        radio: device.lorawan.as_ref().map(HubRadioCtx::from_info),
        radio_control_off: false,
        gateways,
        northbridge: northbridge_ctx(device),
        nb_firmware: device
            .northbridge_diag
            .as_ref()
            .and_then(|(doc, at)| NorthbridgeCardCtx::from_firmware_doc(doc, *at)),
        lorawan: device
            .lorawan_diag
            .as_ref()
            .and_then(|(doc, at)| LorawanDiagCtx::from_doc(doc, *at)),
        lorawan_disabled: lorawan_disabled(device),
        nb_update_allowed: false,
        online: device.online,
        nb_progress: None,
        client: false,
    }
}

/// The Northbridge row of the radio card and the Hub list: `None` without a document,
/// or for one that says `disabled`.
fn northbridge_ctx(device: &DeviceState) -> Option<NorthbridgeCardCtx> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    northbridge_ctx_at(device, now)
}

/// A document no publisher refreshed for this long is not a running P2P radio.
const NORTHBRIDGE_STALE_SECS: i64 = 10 * 60;

fn northbridge_ctx_at(device: &DeviceState, now: i64) -> Option<NorthbridgeCardCtx> {
    let (doc, at) = device.northbridge_diag.as_ref()?;
    // A mode without P2P and a document nobody refreshes means no Northbridge row: a cluster
    // Client's document is published by the Server's central while it lends its Northbridge,
    // and nobody overwrites it with `disabled` once the cluster leaves P2P. A fresh document
    // wins over the mode, which `system/info` reports only once per its interval, so a Hub
    // just switched to P2P shows its radio at once.
    if !reported_mode_allows_p2p(device) && now - *at > NORTHBRIDGE_STALE_SECS {
        return None;
    }
    NorthbridgeCardCtx::from_doc(doc, *at)
}

/// Whether the Hub's own `system/info` (`lorawan.mode`) leaves room for P2P. A Hub that
/// reports no mode yet is given the benefit of the doubt.
fn reported_mode_allows_p2p(device: &DeviceState) -> bool {
    match device.lorawan.as_ref().and_then(|l| l.mode.as_deref()) {
        Some(mode) => matches!(mode.trim().to_ascii_lowercase().as_str(), "p2p" | "dual"),
        None => true,
    }
}

/// The `radio/lorawan` document says `disabled`, or the Hub's own `system/info` reports a mode
/// without LoRaWAN (`p2p`, `off`). The mode matters for a cluster Client, whose document does
/// not reach the Server's broker (#177).
fn lorawan_disabled(device: &DeviceState) -> bool {
    let doc_disabled = device
        .lorawan_diag
        .as_ref()
        .and_then(|(doc, _)| doc.get("state").and_then(|v| v.as_str()))
        == Some("disabled");
    let mode_off = device
        .lorawan
        .as_ref()
        .and_then(|l| l.mode.as_deref())
        .is_some_and(|m| matches!(m.trim().to_ascii_lowercase().as_str(), "p2p" | "off"));
    doc_disabled || mode_off
}

/// Whether the RAK5146 card is fitted: the document's `card.present` (reported in every mode),
/// else, while LoRaWAN runs, v1's `gateway_present`. `None` when the Hub cannot say.
fn rak_card_present(device: &DeviceState) -> Option<bool> {
    let from_doc = device.lorawan_diag.as_ref().and_then(|(doc, _)| {
        doc.get("card")
            .and_then(|c| c.get("present"))
            .and_then(|p| p.as_bool())
    });
    from_doc.or_else(|| {
        if lorawan_disabled(device) {
            None
        } else {
            device.lorawan.as_ref().and_then(|l| l.gateway_present)
        }
    })
}

/// Below this the mPCIe slot's buck (fed from BAT+) cannot hold 3.3 V and the RAK5146 drops
/// off USB, even on DC power (#181).
const RAK_SLOT_MIN_VBAT_MV: i64 = 3500;

/// The RAK5146 is missing because the battery is too low to power its slot: LoRaWAN mode only.
fn rak_card_low_power(device: &DeviceState) -> bool {
    !lorawan_disabled(device)
        && rak_card_present(device) == Some(false)
        && device
            .power
            .battery_voltage_mv
            .is_some_and(|mv| mv > 0 && mv < RAK_SLOT_MIN_VBAT_MV)
}

/// `pages/hub_detail.html`'s Overview "System Status" cards — v1's
/// `ConnectionStatusCard`/`PowerStatusCard`/`NetworkStatusCard`/
/// `SystemInfoCard` (components/dashboard/*.tsx), reproduced as plain data:
/// every field is read straight off the store with NO formatting beyond
/// what `DeviceState` already stores as a display string
/// (`system.uptime_human`) — the template renders "–" for anything `none`,
/// never invents a value. mV -> V conversion and byte formatting stay in
/// the template (`(mv/1000)|round(2)`), same convention `sensor_card.html`
/// already uses for °C rounding.
#[derive(Debug, Clone, Serialize)]
pub struct PowerCardCtx {
    pub percentage: Option<i64>,
    pub status: Option<String>,
    pub voltage_mv: Option<i64>,
    pub dc_connected: bool,
    pub dc_voltage_mv: Option<i64>,
    pub last_dc_loss: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkCardCtx {
    pub wifi_connected: Option<bool>,
    pub wifi_signal_dbm: Option<i64>,
    pub wifi_ip: Option<String>,
    pub ethernet_connected: Option<bool>,
    pub ethernet_ip: Option<String>,
    pub has_internet: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemCardCtx {
    pub host: Option<String>,
    pub label: Option<String>,
    pub version: Option<String>,
    pub uptime: Option<String>,
    pub disk_available_bytes: Option<i64>,
    pub disk_used_percent: Option<f64>,
    /// GitLab #58. Three states, not two: `Some(true)` an operator finished setup,
    /// `Some(false)` nobody has, `None` the Hub has not said — an agent predating the key, or
    /// one that has not published a `system/info` since boot. The template renders the third as
    /// "unknown" rather than as "not set up", because claiming a configured Hub is out of the
    /// box is exactly the error this row exists to stop.
    pub setup_provisioned: Option<bool>,
    pub setup_role: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemStatusCtx {
    pub mqtt_connected: bool,
    pub mqtt_reason: Option<String>,
    /// The RAK5146 card fitted / missing / unknown, for the Connection rows.
    pub rak_card: Option<bool>,
    /// The card is missing for want of battery voltage (#181): a warning, not a fault.
    pub rak_low_power: bool,
    /// Radio in P2P/off: the card is informational only, so its dot stays grey.
    pub lorawan_disabled: bool,
    pub power: PowerCardCtx,
    pub network: NetworkCardCtx,
    pub system: SystemCardCtx,
}

/// Built from the same `DeviceState`/`MqttConnectionState` the dashboard
/// already reads — no new store fields, just a different template's worth
/// of them. `system.host`/`version` prefer the `system` block (refreshed by
/// `system/info`) and fall back to the top-level `DeviceState` fields the
/// same way [`device_page_ctx`]'s `device_label` does, since a device that
/// hasn't sent a fresh `system/info` yet still has an identity.
pub fn system_status_ctx(device: &DeviceState, mqtt: &MqttConnectionState) -> SystemStatusCtx {
    SystemStatusCtx {
        mqtt_connected: mqtt.connected,
        mqtt_reason: mqtt.last_disconnect_reason.clone(),
        rak_card: rak_card_present(device),
        rak_low_power: rak_card_low_power(device),
        lorawan_disabled: lorawan_disabled(device),
        power: PowerCardCtx {
            percentage: device.power.battery_percentage,
            status: device.power.battery_status.clone(),
            voltage_mv: device.power.battery_voltage_mv,
            dc_connected: device.power.dc_connected.unwrap_or(false),
            dc_voltage_mv: device.power.dc_voltage_mv,
            last_dc_loss: device.power.last_dc_loss.clone(),
        },
        network: NetworkCardCtx {
            wifi_connected: device.network.wifi_connected,
            wifi_signal_dbm: device.network.wifi_signal_dbm,
            wifi_ip: device.network.wifi_ip.clone(),
            ethernet_connected: device.network.ethernet_connected,
            ethernet_ip: device.network.ethernet_ip.clone(),
            has_internet: device.network.has_internet,
        },
        system: SystemCardCtx {
            host: device
                .system
                .hostname
                .clone()
                .or_else(|| Some(device.hostname.clone())),
            label: device
                .system
                .device_label
                .clone()
                .or_else(|| device.config.device_label.clone()),
            version: device
                .system
                .version
                .clone()
                .or_else(|| device.version.clone()),
            uptime: device.system.uptime_human.clone(),
            disk_available_bytes: device.system.storage.available_bytes,
            disk_used_percent: device.system.storage.used_percent,
            // Straight through from what the device published. The portal keeps no copy and
            // derives nothing: the value's home is `setup:` in the Hub's own config
            // (decision #4), and anything computed here would be a second source of truth.
            setup_provisioned: device.system.setup_provisioned,
            setup_role: device.system.setup_role.clone(),
        },
    }
}

/// `partials/hub_unit_settings.html`'s `display_config` — the five scalar
/// display settings the Hub last reported, straight off `config/state`
/// (plan/portal/display-config.md §4 seam contract). Every field is
/// nullable: a Hub that has not sent a `config/state` yet leaves the
/// controls at their browser defaults rather than at an invented value.
#[derive(Debug, Clone, Serialize, Default)]
pub struct DisplayConfigCtx {
    pub buzzer_volume: Option<i64>,
    pub screen_brightness: Option<i64>,
    pub led_brightness: Option<i64>,
    /// 0 = always on.
    pub screen_timeout_secs: Option<i64>,
    pub device_label: Option<String>,
}

/// [`DisplayConfigCtx`] from the ingested config state.
pub fn display_config_ctx(config: &DeviceConfigState) -> DisplayConfigCtx {
    DisplayConfigCtx {
        buzzer_volume: config.buzzer_volume,
        screen_brightness: config.screen_brightness,
        led_brightness: config.led_brightness,
        screen_timeout_secs: config.screen_timeout_secs,
        device_label: config.device_label.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::test_support::sensor;
    use crate::state::models::SensorAlarmCounts;

    /// Bench test 2026-10-29: a Hub reporting `lorawan` hides a P2P row left by a stale
    /// retained Northbridge document (not a fresh one), and its Northbridge firmware card
    /// stays, in any mode.
    #[test]
    fn a_lorawan_hub_shows_no_p2p_row_but_keeps_the_nb_firmware_card() {
        let doc = serde_json::json!({
            "state": "alive",
            "fw_version": "0.4.2",
            "proto_version": 0,
        });
        for (mode, p2p_row) in [
            ("lorawan", false),
            ("off", false),
            ("p2p", true),
            ("dual", true),
        ] {
            let mut device = DeviceState::new("proximos-2200ad33".into());
            device.lorawan = Some(fiber_protocol::payloads::system_info::LorawanGatewayInfo {
                mode: Some(mode.into()),
                ..Default::default()
            });
            let at = 1_793_300_000;
            device.northbridge_diag = Some((doc.clone(), at));
            let stale = at + NORTHBRIDGE_STALE_SECS + 1;
            assert_eq!(
                northbridge_ctx_at(&device, stale).is_some(),
                p2p_row,
                "{mode}"
            );
            // Fresh, it shows in any mode: the mode report lags a switch to P2P.
            assert!(
                northbridge_ctx_at(&device, at + 30).is_some(),
                "fresh in {mode}"
            );
            let card = radio_card_ctx(&device, None, &[]);
            assert!(card.nb_firmware.is_some(), "the firmware card in {mode}");
        }
    }

    /// UI-26 on a Server: its own gateway and its Clients' are listed, the Client's named;
    /// a gateway that belongs to neither is not.
    #[test]
    fn the_servers_card_lists_its_clients_gateways() {
        use crate::state::radio::{ApiStatus, GatewayStatus, GatewaysReport};
        let gw = |eui: &str| GatewayStatus {
            gateway_eui: eui.into(),
            name: eui.into(),
            hostname: None,
            ip: None,
            status: "online".into(),
            last_seen: None,
            rssi: None,
            snr: None,
            radio: "lorawan".into(),
        };
        let mut device = DeviceState::new("proximos-4e970f4a".into());
        device.lorawan = Some(fiber_protocol::payloads::system_info::LorawanGatewayInfo {
            gateway_eui: Some("0016c001f136c66a".into()),
            ..Default::default()
        });
        let gateways = RadioGatewaysState {
            report: GatewaysReport {
                timestamp: String::new(),
                api_status: ApiStatus::Ok,
                gateways: vec![
                    gw("0016c001f136c66a"),
                    gw("0016c001f136e508"),
                    gw("00000000000000ff"),
                ],
            },
            received_at: 0,
        };
        let clients = [(
            "0016c001f136e508".to_string(),
            "proximos-2200ad33".to_string(),
        )];
        let rows = radio_card_ctx(&device, Some(&gateways), &clients)
            .gateways
            .expect("card")
            .rows;
        let got: Vec<(&str, bool, Option<&str>)> = rows
            .iter()
            .map(|r| (r.gateway_eui.as_str(), r.local, r.client.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("0016c001f136c66a", true, None),
                ("0016c001f136e508", false, Some("proximos-2200ad33")),
            ]
        );
    }

    #[test]
    fn casing_map_covers_all_firmware_states() {
        let mut s = sensor(0);
        s.connected = true;
        s.updated_at = Some(1);

        for (fw, want) in [
            ("NORMAL", "Normal"),
            ("WARNING", "Warning"),
            ("CRITICAL", "Critical"),
            ("DISCONNECTED", "Disconnected"),
            ("RECONNECTING", "Reconnecting"),
            ("NEVERCONNECTED", "NeverConnected"),
        ] {
            s.dominant_alarm_state = fw.to_string();
            assert_eq!(normalize_alarm_state(&s), want, "firmware state {fw}");
        }
    }

    #[test]
    fn never_updated_fallback_overrides_stored_default() {
        // Freshly pre-created line: nothing has actually reported for it
        // yet (updated_at/sample_count/connected all at their zero value,
        // whatever the raw dominant_alarm_state string happens to
        // default to) — should read as NeverConnected regardless.
        let s = sensor(2);
        assert!(s.updated_at.is_none());
        assert_eq!(s.sample_count, 0);
        assert!(!s.connected);

        assert_eq!(normalize_alarm_state(&s), "NeverConnected");
    }

    #[test]
    fn updated_line_reporting_normal_is_not_never_connected() {
        let mut s = sensor(2);
        s.updated_at = Some(100);
        s.sample_count = 5;
        s.connected = true;
        s.dominant_alarm_state = "NORMAL".to_string();

        assert_eq!(normalize_alarm_state(&s), "Normal");
    }

    #[test]
    fn unrecognized_firmware_state_passes_through() {
        let mut s = sensor(0);
        s.connected = true;
        s.updated_at = Some(1);
        s.dominant_alarm_state = "SOME_NEW_STATE".to_string();

        assert_eq!(normalize_alarm_state(&s), "SOME_NEW_STATE");
    }

    #[test]
    fn sensor_card_ctx_maps_fields() {
        let mut s = sensor(3);
        s.name = Some("Freezer".into());
        s.location = Some("Shelf A".into());
        s.connected = true;
        s.temperature_avg_celsius = Some(-18.2);
        s.dominant_alarm_state = "WARNING".to_string();
        s.updated_at = Some(3661); // 01:01:01
        s.alarm_counts = SensorAlarmCounts::default();

        let ctx = sensor_card_ctx("hub1", &s, false);
        assert_eq!(ctx.hostname, "hub1");
        assert_eq!(ctx.line, 3);
        assert_eq!(ctx.name.as_deref(), Some("Freezer"));
        assert_eq!(ctx.dominant_alarm_state, "Warning");
        assert_eq!(ctx.avg_c, Some(-18.2));
        assert_eq!(ctx.updated_at.as_deref(), Some("01:01:01 UTC"));
        assert!(!ctx.can_edit);
        assert!(ctx.thresholds.is_none());
    }

    #[test]
    fn sensor_card_ctx_threading_can_edit_and_thresholds() {
        let mut s = sensor(1);
        s.thresholds = Some(crate::state::SensorThresholds {
            critical_low: Some(-25.0),
            warning_low: Some(-20.0),
            warning_high: Some(-5.0),
            critical_high: Some(0.0),
            alarm_low: None,
            alarm_high: None,
            hysteresis: None,
            enabled: None,
        });

        let ctx = sensor_card_ctx("hub1", &s, true);
        assert!(ctx.can_edit);
        let t = ctx.thresholds.expect("thresholds present");
        assert_eq!(t.critical_low, Some(-25.0));
        assert_eq!(t.warning_high, Some(-5.0));
        assert_eq!(t.alarm_low, None);
    }

    #[test]
    fn device_page_ctx_unpaired() {
        let ctx = device_page_ctx(None, None, None, false, false);
        let pairing = ctx.get_attr("pairing").unwrap();
        assert!(!pairing.get_attr("paired").unwrap().is_true());
        assert!(pairing.get_attr("ca_fingerprint").unwrap().is_none());
        assert!(ctx.get_attr("hostname").unwrap().is_none());
        assert!(!ctx.get_attr("online").unwrap().is_true());
        assert!(!ctx.get_attr("local_device").unwrap().is_true());
    }

    #[test]
    fn device_page_ctx_marks_the_portals_own_hub_as_local() {
        // Drives the codeless pair form in `pages/hub_detail.html`.
        let device = DeviceState::new("fiber-01".into());

        let ctx = device_page_ctx(Some(&device), None, None, false, true);

        assert!(ctx.get_attr("local_device").unwrap().is_true());
    }

    #[test]
    fn device_page_ctx_paired_shows_fingerprint_and_paired_by() {
        let device = DeviceState::new("fiber-01".into());
        let info = DeviceIdentityInfo {
            hostname: "fiber-01".into(),
            has_ca: true,
            ca_id: Some("ca-1".into()),
            ca_public_key: "aa".repeat(32),
            paired_by_user_id: Some(1),
            paired_at: "2026-01-01 00:00:00".into(),
        };

        let ctx = device_page_ctx(
            Some(&device),
            Some(&info),
            Some("alice".into()),
            false,
            false,
        );
        let pairing = ctx.get_attr("pairing").unwrap();
        assert!(pairing.get_attr("paired").unwrap().is_true());
        assert_eq!(pairing.get_attr("paired_by").unwrap().to_string(), "alice");
        let fp = pairing.get_attr("ca_fingerprint").unwrap().to_string();
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        // Deterministic for the same public key.
        assert_eq!(ca_fingerprint(&info.ca_public_key), fp);
    }

    /// GitLab #61: a pairing the Hub asked for has no user to name, and "unknown" is what a
    /// *lost* record looks like. The flag is what lets the page tell an operator which it is.
    #[test]
    fn device_page_ctx_says_when_a_pairing_had_no_user_behind_it() {
        let device = DeviceState::new("fiber-01".into());
        let info = DeviceIdentityInfo {
            hostname: "fiber-01".into(),
            has_ca: true,
            ca_id: Some("ca-1".into()),
            ca_public_key: "aa".repeat(32),
            paired_by_user_id: None,
            paired_at: "2026-01-01 00:00:00".into(),
        };

        let ctx = device_page_ctx(Some(&device), Some(&info), None, true, false);
        let pairing = ctx.get_attr("pairing").unwrap();
        assert!(pairing.get_attr("paired").unwrap().is_true());
        assert!(pairing.get_attr("paired_via_invite").unwrap().is_true());
        assert!(
            pairing.get_attr("paired_by").unwrap().is_none(),
            "there is no user to name, and inventing one would be the lie this flag avoids"
        );
    }

    /// UX-005: on the portal's own Hub with an inert radio seam the card
    /// adds the missing fact and keeps the Hub's all-green report beside it.
    #[test]
    fn the_radio_card_says_when_this_portal_cannot_command_nodes() {
        let mut device = DeviceState::new("proximos-0001".into());
        device.lorawan = Some(fiber_protocol::payloads::system_info::LorawanGatewayInfo {
            gateway_present: Some(true),
            concentratord_running: Some(true),
            chirpstack_running: Some(true),
            ..Default::default()
        });
        let render = |radio_control_off: bool| {
            let ctx = RadioCardCtx {
                radio_control_off,
                ..radio_card_ctx(&device, None, &[])
            };
            crate::render::Templates::new("templates", "static")
                .env()
                .get_template("partials/hub_radio_card.html")
                .expect("template loads")
                .render(minijinja::context! { lang => "cs", radio => ctx })
                .expect("renders")
        };

        let off = render(true);
        assert!(off.contains("Z tohoto portálu nejde posílat příkazy Nodům"));
        assert!(off.contains("concentratord ✓"), "the Hub's report stays");
        assert!(off.contains("chirpstack ✓"));

        assert!(!render(false).contains("Z tohoto portálu nejde posílat příkazy Nodům"));
    }

    /// #181: a missing card on a flat battery is a power warning, in LoRaWAN mode only.
    #[test]
    fn a_missing_card_on_a_low_battery_is_a_power_warning() {
        let hub = |present: bool, vbat_mv: i64, state: &str| {
            let mut device = DeviceState::new("proximos-dc9cb4a8".into());
            device.power.battery_voltage_mv = Some(vbat_mv);
            device.lorawan_diag = Some((
                serde_json::json!({ "state": state, "card": { "present": present } }),
                1_700_000_000,
            ));
            rak_card_low_power(&device)
        };
        assert!(hub(false, 3312, "alive"));
        assert!(
            !hub(false, 3700, "alive"),
            "a healthy battery leaves the card fault red"
        );
        assert!(!hub(true, 3312, "alive"));
        assert!(!hub(false, 3312, "disabled"), "nothing in P2P mode");
    }

    /// control/cluster.md §2: a Client runs no ChirpStack, so neither the Hubs list row nor
    /// the radio card draws a chirpstack chip (or diagnostics row) for it; a Server keeps it.
    #[test]
    fn a_client_hub_shows_no_chirpstack_status() {
        let mut device = DeviceState::new("proximos-2200ad33".into());
        device.online = true;
        device.lorawan = Some(fiber_protocol::payloads::system_info::LorawanGatewayInfo {
            gateway_present: Some(true),
            concentratord_running: Some(true),
            chirpstack_running: Some(false),
            ..Default::default()
        });
        device.lorawan_diag = Some((
            serde_json::json!({
                "state": "alive",
                "card": { "present": true },
                "concentratord": { "unit": "active" },
                "forwarder": { "unit": "active", "conn": "online" },
                "chirpstack": { "unit": "inactive", "result": "success" },
            }),
            1_700_000_000,
        ));
        let env = crate::render::Templates::new("templates", "static").env();

        let card = |client: bool| {
            let mut ctx = radio_card_ctx(&device, None, &[]);
            ctx.client = client;
            ctx.lorawan = None;
            let chips = env
                .get_template("partials/hub_radio_card.html")
                .expect("template loads")
                .render(minijinja::context! { lang => "cs", radio => ctx })
                .expect("renders");
            let mut ctx = radio_card_ctx(&device, None, &[]);
            ctx.client = client;
            let rows = env
                .get_template("partials/hub_radio_card.html")
                .expect("template loads")
                .render(minijinja::context! { lang => "cs", radio => ctx })
                .expect("renders");
            (chips, rows)
        };
        let (chips, rows) = card(true);
        assert!(chips.contains("concentratord ✓"));
        assert!(
            !chips.contains("chirpstack"),
            "no chip on a Client: {chips}"
        );
        assert!(!rows.contains("inactive"), "no chirpstack row on a Client");
        let (chips, rows) = card(false);
        assert!(chips.contains("chirpstack ✕"), "a Server keeps the chip");
        assert!(rows.contains("inactive"), "a Server keeps the row");

        let list = |kind: &'static str| {
            let row = HubRowCtx {
                kind: Some(kind),
                ..hub_row_ctx(&device, &crate::notify::HubFaults::default())
            };
            env.get_template("pages/hubs.html")
                .expect("template loads")
                .render(minijinja::context! {
                    lang => "cs",
                    hubs => vec![row],
                    show_type => true,
                })
                .expect("renders")
        };
        let client = list("client");
        assert!(client.contains(r#"data-part="card""#));
        assert!(
            !client.contains(r#"data-part="chirpstack""#),
            "no ChirpStack line on a Client row"
        );
        assert!(list("server").contains(r#"data-part="chirpstack""#));
    }

    /// devices.hub.md §2.1: the triangle's tooltip names what is on, so an online Hub whose
    /// portal partition is filling up does not read as offline.
    #[test]
    fn the_fault_tooltip_names_each_condition() {
        use crate::notify::{HubFaults, SystemAlertKind};
        let faults = HubFaults::default();
        let mut device = DeviceState::new("proximos-4e970f4a".into());
        device.online = true;
        assert!(hub_fault_keys(&device, &faults).is_empty());

        faults.set(
            "proximos-4e970f4a",
            vec![
                SystemAlertKind::StorageLow,
                SystemAlertKind::PortalStorageLow,
            ],
        );
        assert_eq!(
            hub_fault_keys(&device, &faults),
            [
                "alarms.event_type.storage_low",
                "alarms.event_type.viewer_storage_low"
            ]
        );

        // Offline leads; the notifier's own offline condition is not said twice, and while
        // the store already has the Hub back it is not said at all.
        faults.set(
            "proximos-4e970f4a",
            vec![SystemAlertKind::DeviceOffline, SystemAlertKind::StorageLow],
        );
        device.online = false;
        assert_eq!(
            hub_fault_keys(&device, &faults),
            [
                "monitoring.hubs.fault_offline",
                "alarms.event_type.storage_low"
            ]
        );
        device.online = true;
        assert_eq!(
            hub_fault_keys(&device, &faults),
            ["alarms.event_type.storage_low"]
        );
    }

    fn cm4() -> fiber_protocol::payloads::system_info::Hardware {
        fiber_protocol::payloads::system_info::Hardware {
            model: Some("Raspberry Pi Compute Module 4".into()),
            revision: Some("1.1".into()),
            serial: Some("100000004e970f4a".into()),
        }
    }

    fn with_mode(device: &mut DeviceState, mode: &str) {
        device.lorawan = Some(fiber_protocol::payloads::system_info::LorawanGatewayInfo {
            mode: Some(mode.into()),
            gateway_eui: Some("70b3d54900aa0001".into()),
            ..Default::default()
        });
    }

    /// devices.hub.md §3.1 (amendment 2026-09-28): a CM4 on LoRaWAN, every fact reported.
    #[test]
    fn hub_description_ctx_reads_hardware_and_lorawan() {
        let mut device = DeviceState::new("proximos-4e970f4a".into());
        device.system.hardware = Some(cm4());
        with_mode(&mut device, "lorawan");

        let ctx = hub_description_ctx(&device);
        assert_eq!(ctx.serial.as_deref(), Some("100000004e970f4a"));
        assert_eq!(
            ctx.model_revision.as_deref(),
            Some("Raspberry Pi Compute Module 4 / 1.1")
        );
        assert_eq!(ctx.radio_mode, Some("lorawan"));
        assert_eq!(ctx.radio_mode_other, None);
        // The list row's SN is the same serial.
        assert_eq!(
            hub_row_ctx(&device, &crate::notify::HubFaults::default())
                .serial
                .as_deref(),
            Some("100000004e970f4a")
        );
    }

    /// Every mode this build knows is labelled, with no other mode reported.
    #[test]
    fn hub_description_ctx_labels_every_known_mode() {
        let mut device = DeviceState::new("h1".into());
        for mode in ["off", "lorawan", "p2p", "dual"] {
            with_mode(&mut device, mode);
            let ctx = hub_description_ctx(&device);
            assert_eq!(ctx.radio_mode, Some(mode));
            assert_eq!(ctx.radio_mode_other, None, "{mode}");
        }
    }

    /// A mode this build does not label is passed through verbatim, never as an i18n key.
    #[test]
    fn hub_description_ctx_passes_an_unknown_mode_through() {
        let mut device = DeviceState::new("h1".into());
        with_mode(&mut device, "mesh");
        let ctx = hub_description_ctx(&device);
        assert_eq!(ctx.radio_mode, None);
        assert_eq!(ctx.radio_mode_other.as_deref(), Some("mesh"));
    }

    /// An agent that predates `hardware` and `mode`: every fact is `None` ("—"). A model
    /// without a revision is the model alone.
    #[test]
    fn hub_description_ctx_without_hardware_is_all_unknown() {
        let mut device = DeviceState::new("h1".into());
        let ctx = hub_description_ctx(&device);
        assert_eq!(ctx.serial, None);
        assert_eq!(ctx.model_revision, None);
        assert_eq!(ctx.radio_mode, None);
        assert_eq!(ctx.radio_mode_other, None);
        assert_eq!(
            hub_row_ctx(&device, &crate::notify::HubFaults::default()).serial,
            None
        );

        device.system.hardware = Some(fiber_protocol::payloads::system_info::Hardware {
            model: Some("Raspberry Pi Compute Module 4".into()),
            ..Default::default()
        });
        assert_eq!(
            hub_description_ctx(&device).model_revision.as_deref(),
            Some("Raspberry Pi Compute Module 4")
        );
    }

    /// The northbridge row of the TOWER sample with `update` replaced.
    fn nb_with_update(update: serde_json::Value) -> NorthbridgeCardCtx {
        let mut doc: serde_json::Value = serde_json::from_str(TOWER_NB_SAMPLE).unwrap();
        doc["update"] = update;
        NorthbridgeCardCtx::from_doc(&doc, 0).expect("a row")
    }

    /// northbridge_update.md §3.4 (amended 2026-10-09): the card follows a manual update from
    /// the request to its outcome — running while the Hub has not started (within the grace)
    /// or reports `flashing`, then the document read as the outcome; nothing once too old.
    #[test]
    fn a_followed_northbridge_update_runs_then_reads_the_document_as_its_outcome() {
        use NbUpdatePhase::{Done, Run, Wait};
        let idle =
            nb_with_update(serde_json::json!({ "state": "idle", "bundled": "0.3.0+c05b8d3e" }));
        let flashing =
            nb_with_update(serde_json::json!({ "state": "flashing", "bundled": "0.3.0+c05b8d3e" }));
        let failed = nb_with_update(serde_json::json!({
            "state": "idle", "bundled": "0.3.0+c05b8d3e", "attempts": 1,
            "last_error": "attempt 1 of 3 to flash 0.3.0+c05b8d3e failed: no ACK"
        }));
        let differs =
            nb_with_update(serde_json::json!({ "state": "idle", "bundled": "0.4.0+1a2b3c4d" }));
        let since = 1_000;

        // Just asked: the Hub has not picked it up yet.
        let p = nb_update_progress(Some(&idle), Wait, since, since + 5).expect("followed");
        assert_eq!((p.outcome, p.poll), ("running", true));
        assert_eq!(p.query, "?nbu=wait&since=1000");
        // The Hub flashes: the phase moves on.
        let p = nb_update_progress(Some(&flashing), Wait, since, since + 8).expect("followed");
        assert_eq!((p.outcome, p.poll), ("running", true));
        assert_eq!(p.query, "?nbu=run&since=1000");
        // Back to idle on the bundled image: done, with the version.
        let p = nb_update_progress(Some(&idle), Run, since, since + 50).expect("followed");
        assert_eq!((p.outcome, p.poll), ("ok", false));
        assert_eq!(p.detail.as_deref(), Some("0.3.0+c05b8d3e"));
        assert_eq!(p.query, "?nbu=done&since=1000");
        // A failed attempt: the Hub's own sentence.
        let p = nb_update_progress(Some(&failed), Run, since, since + 50).expect("followed");
        assert_eq!(p.outcome, "failed");
        assert!(p.detail.as_deref().is_some_and(|d| d.contains("no ACK")));
        // Never started within the grace, still on another image: unconfirmed.
        let p = nb_update_progress(Some(&differs), Wait, since, since + NB_UPDATE_START_GRACE_S)
            .expect("followed");
        assert_eq!((p.outcome, p.poll), ("unconfirmed", false));
        // The row is gone (mode switched): unconfirmed, not a success.
        let p = nb_update_progress(None, Done, since, since + 100).expect("followed");
        assert_eq!(p.outcome, "unconfirmed");
        // Too old, or from the future: not followed.
        assert!(
            nb_update_progress(Some(&idle), Done, since, since + NB_UPDATE_FOLLOW_S + 1).is_none()
        );
        assert!(nb_update_progress(Some(&idle), Wait, since, since - 1).is_none());
    }

    fn sb(
        state: Option<&str>,
        last_result: Option<&str>,
        last_error: Option<&str>,
    ) -> SouthbridgeCtx {
        SouthbridgeCtx {
            version: Some("south-zephyr 1.1.0 g73f39ed5".into()),
            bundled: Some("south-zephyr 1.1.0 g73f39ed5".into()),
            has_update: true,
            state: state.map(str::to_string),
            attempts: Some(0),
            last_error: last_error.map(str::to_string),
            last_result: last_result.map(str::to_string),
        }
    }

    /// southbridge_update.md §3.7: the card follows a manual update from the request to its
    /// outcome — running while the Hub has not started (within the grace) or reports
    /// `running`, then the block read as the outcome; nothing once too old.
    #[test]
    fn a_followed_southbridge_update_runs_then_reads_the_block_as_its_outcome() {
        use NbUpdatePhase::{Done, Run, Wait};
        let since = 1_000;
        let idle_old = sb(Some("idle"), None, None);
        let running = sb(Some("running"), None, None);
        let ok = sb(Some("idle"), Some("ok south-zephyr 1.1.0 g73f39ed5"), None);
        let failed = sb(Some("failed"), Some("failed"), Some("no ACK after erase"));

        let p = sb_update_progress(Some(&idle_old), Wait, since, since + 2).expect("followed");
        assert_eq!((p.outcome, p.poll), ("running", true));
        assert_eq!(p.query, "?sbu=wait&since=1000");
        let p = sb_update_progress(Some(&running), Wait, since, since + 4).expect("followed");
        assert_eq!((p.outcome, p.poll), ("running", true));
        assert_eq!(p.query, "?sbu=run&since=1000");
        let p = sb_update_progress(Some(&ok), Run, since, since + 20).expect("followed");
        assert_eq!((p.outcome, p.poll), ("ok", false));
        assert_eq!(p.detail.as_deref(), Some("south-zephyr 1.1.0 g73f39ed5"));
        assert_eq!(p.query, "?sbu=done&since=1000");
        let p = sb_update_progress(Some(&failed), Run, since, since + 20).expect("followed");
        assert_eq!(p.outcome, "failed");
        assert_eq!(p.detail.as_deref(), Some("no ACK after erase"));
        let p = sb_update_progress(
            Some(&idle_old),
            Wait,
            since,
            since + SB_UPDATE_START_GRACE_S,
        )
        .expect("followed");
        assert_eq!((p.outcome, p.poll), ("unconfirmed", false));
        let p = sb_update_progress(None, Done, since, since + 100).expect("followed");
        assert_eq!(p.outcome, "unconfirmed");
        assert!(
            sb_update_progress(Some(&ok), Done, since, since + SB_UPDATE_FOLLOW_S + 1).is_none()
        );
        assert!(sb_update_progress(Some(&ok), Wait, since, since - 1).is_none());
    }

    /// The button: admin, an update object, a bundled image, not running, not polling.
    #[test]
    fn the_sb_update_is_offered_only_where_it_can_act() {
        use fiber_protocol::payloads::system_info::{Southbridge, SouthbridgeUpdate};
        let mut device = DeviceState::new("h1".into());
        let block = |bundled: Option<&str>, update: Option<&str>| Southbridge {
            version: Some("south-zephyr 1.0.0 g11111111".into()),
            bundled: bundled.map(str::to_string),
            update: update.map(|s| SouthbridgeUpdate {
                state: Some(s.into()),
                ..SouthbridgeUpdate::default()
            }),
        };
        let b = Some("south-zephyr 1.1.0 g73f39ed5");
        device.system.southbridge = Some(block(b, Some("idle")));
        assert!(sb_firmware_ctx(&device, true, None, 0).offer_update);
        assert!(!sb_firmware_ctx(&device, false, None, 0).offer_update);
        assert!(!sb_firmware_ctx(&device, true, Some((NbUpdatePhase::Wait, 0)), 5).offer_update);
        for (case, sb) in [
            ("failed is offered", block(b, Some("failed"))),
            ("running", block(b, Some("running"))),
            ("no bundle", block(None, Some("idle"))),
            ("no update", block(b, None)),
        ] {
            device.system.southbridge = Some(sb);
            assert_eq!(
                sb_firmware_ctx(&device, true, None, 0).offer_update,
                case == "failed is offered",
                "{case}"
            );
        }
        device.system.southbridge = None;
        let ctx = sb_firmware_ctx(&device, true, None, 0);
        assert!(ctx.sb.is_none() && !ctx.offer_update);
    }

    /// The TOWER central's retained `radio/northbridge` document: a copy of
    /// `crates/control-radio/tests/fixtures/northbridge_retained_sample.json`, which the
    /// route test reads too.
    const TOWER_NB_SAMPLE: &str =
        include_str!("../../tests/fixtures/northbridge_retained_sample.json");
    /// The same central with `radio.mode` not allowing P2P: every card key at `null` / `0`.
    const TOWER_NB_DISABLED: &str =
        include_str!("../../tests/fixtures/northbridge_disabled_sample.json");

    fn nb_ctx(doc: serde_json::Value) -> NorthbridgeCardCtx {
        NorthbridgeCardCtx::from_doc(&doc, 1_790_568_000).expect("a row")
    }

    fn counter_names(ctx: &NorthbridgeCardCtx) -> Vec<&str> {
        ctx.counters.iter().map(|(k, _)| k.as_str()).collect()
    }

    /// northbridge_update.md §3.5: the firmware line compares the running image with the
    /// bundled one; a Hub without bundled firmware (`update: null`) shows no line.
    #[test]
    fn the_firmware_line_compares_the_running_image_with_the_bundled_one() {
        let sample: serde_json::Value = serde_json::from_str(TOWER_NB_SAMPLE).unwrap();
        let u = nb_ctx(sample.clone())
            .update
            .expect("the sample bundles 0.3.0");
        assert_eq!(u.running.as_deref(), Some("0.3.0+c05b8d3e"));
        assert_eq!(u.bundled.as_deref(), Some("0.3.0+c05b8d3e"));
        assert_eq!((u.verdict, u.badge), ("current", "normal"));

        let mut doc = sample.clone();
        doc["update"]["bundled"] = "0.4.1+1a2b3c4d".into();
        let u = nb_ctx(doc.clone()).update.unwrap();
        assert_eq!((u.verdict, u.badge), ("differs", "warning"));

        doc["update"]["state"] = "failed".into();
        doc["update"]["attempts"] = 3.into();
        doc["update"]["last_error"] = "erase: NACK".into();
        let u = nb_ctx(doc).update.unwrap();
        assert_eq!(u.verdict, "failed");
        assert_eq!(
            (u.attempts, u.last_error.as_deref()),
            (3, Some("erase: NACK"))
        );

        let mut doc = sample;
        doc["update"] = serde_json::Value::Null;
        assert!(nb_ctx(doc).update.is_none());
    }

    /// #158: a Hub without P2P reports the Northbridge firmware from its idle task in a
    /// `disabled` document. The firmware card takes it; the radio row stays hidden, and an
    /// older Hub's `disabled` document still claims nothing.
    #[test]
    fn an_idle_disabled_document_feeds_only_the_firmware_card() {
        let idle = serde_json::json!({
            "state": "disabled", "nb_task": "idle", "fw_version": "0.4.0", "proto_version": 0,
            "update": { "state": "ok", "bundled": "0.4.1+1a2b3c4d", "attempts": 0,
                        "last_error": null, "note": null }
        });
        assert!(NorthbridgeCardCtx::from_doc(&idle, 0).is_none());
        let fw = NorthbridgeCardCtx::from_firmware_doc(&idle, 0).expect("the card");
        assert_eq!(fw.fw_version.as_deref(), Some("0.4.0"));
        assert!(fw.update.is_some());
        let old = serde_json::json!({ "state": "disabled", "fw_version": null, "update": null });
        assert!(NorthbridgeCardCtx::from_firmware_doc(&old, 0).is_none());
    }

    /// A 0.2.x Hub keeps today's row: `proto 1`, its own `radio.state` verbatim, every
    /// counter it sends in the document's order, and no TOWER facts.
    #[test]
    fn a_v02_northbridge_document_keeps_todays_row() {
        let ctx = nb_ctx(serde_json::json!({
            "state": "alive", "fw_version": "0.2.0", "proto_version": 1,
            "radio": { "state": "rx", "freq_hz": 869_525_000, "sf": 7, "tx_power_dbm": 14 },
            "counters": { "rx": 10, "tx": 9, "tx_refused": 0 }
        }));
        assert_eq!(ctx.state_key, Some("monitoring.hubs.radio_state.alive"));
        assert_eq!(ctx.proto_version, Some(1));
        assert_eq!((ctx.git_sha.clone(), ctx.build_flags.len()), (None, 0));
        assert_eq!(ctx.radio_state.as_deref(), Some("rx"));
        assert_eq!(ctx.radio_state_key, None, "0.2.x states stay verbatim");
        assert!(ctx.gateway.is_none());
        assert_eq!(counter_names(&ctx), ["rx", "tx", "tx_refused"]);
    }

    /// The TOWER document: commit on the FW line, `gateway` as a mode label, the gateway's
    /// net / Nodes / queue, and a curated counter set that keeps the central's `tx_refused`
    /// even at 0.
    #[test]
    fn a_tower_northbridge_document_maps_its_new_keys() {
        let ctx = nb_ctx(serde_json::from_str(TOWER_NB_SAMPLE).unwrap());
        assert_eq!(ctx.fw_version.as_deref(), Some("0.3.0"));
        assert_eq!(ctx.proto_version, None, "0 is the flag day, not a version");
        assert_eq!(ctx.git_sha.as_deref(), Some("c05b8d3e"));
        assert!(ctx.build_flags.is_empty(), "a release image");
        assert_eq!(
            ctx.radio_state_key,
            Some("monitoring.hubs.radio_nb_mode.gateway")
        );
        let g = ctx.gateway.clone().expect("gateway");
        assert_eq!(g.net_id, Some(0x6d24_8a62));
        assert_eq!((g.peers, g.peers_max), (Some(2), Some(64)));
        assert_eq!((g.queue_used, g.queue_max), (Some(1), Some(32)));
        assert_eq!(
            counter_names(&ctx),
            [
                "rx",
                "tx",
                "crc_err",
                "link_err",
                "rx_rearm_fail",
                "tx_radio_err",
                "tx_refused",
                "rx_mic_fail",
                "rx_dup"
            ]
        );
        assert!(ctx.counters.contains(&("tx_refused".to_string(), 0)));
    }

    /// `net_id` 0 (before `TWR_START`) is not a network; `boot` and `configured` are labelled;
    /// build flags come through.
    #[test]
    fn tower_early_states_and_build_flags() {
        let mut doc: serde_json::Value = serde_json::from_str(TOWER_NB_SAMPLE).unwrap();
        doc["gateway"]["net_id"] = 0.into();
        doc["radio"]["state"] = "boot".into();
        doc["build_flags"] = serde_json::json!(["bench_rtt_bridge", "debug"]);
        let ctx = nb_ctx(doc.clone());
        assert_eq!(ctx.gateway.and_then(|g| g.net_id), None);
        assert_eq!(
            ctx.radio_state_key,
            Some("monitoring.hubs.radio_nb_mode.boot")
        );
        assert_eq!(ctx.build_flags, ["bench_rtt_bridge", "debug"]);
        doc["radio"]["state"] = "configured".into();
        assert_eq!(
            nb_ctx(doc).radio_state_key,
            Some("monitoring.hubs.radio_nb_mode.configured")
        );
    }

    /// `starting` before the first `GET_INFO`: the central's document has every card key at
    /// `null` / `0` and no `gateway` — the row shows the state and nothing it does not know.
    #[test]
    fn a_tower_document_before_get_info_shows_no_numbers() {
        let mut doc: serde_json::Value = serde_json::from_str(TOWER_NB_DISABLED).unwrap();
        doc["state"] = "starting".into();
        doc["uart"] = "/dev/ttyAMA3".into();
        let ctx = nb_ctx(doc);
        assert_eq!(ctx.state_key, Some("monitoring.hubs.radio_state.starting"));
        assert_eq!(
            (ctx.fw_version, ctx.uptime, ctx.proto_version),
            (None, None, None)
        );
        assert!(ctx.counters.is_empty(), "the zeros are placeholders");
        assert!(ctx.gateway.is_none());
        assert_eq!((ctx.radio_state, ctx.freq_hz, ctx.sf), (None, None, None));
    }

    /// `disabled` hides the row (§4), in the central's full-key shape too.
    #[test]
    fn a_disabled_tower_document_has_no_row() {
        let doc: serde_json::Value = serde_json::from_str(TOWER_NB_DISABLED).unwrap();
        assert!(NorthbridgeCardCtx::from_doc(&doc, 0).is_none());
    }

    /// A state this build does not know is shown verbatim: no key for the template.
    #[test]
    fn unknown_northbridge_states_have_no_key() {
        let mut doc: serde_json::Value = serde_json::from_str(TOWER_NB_SAMPLE).unwrap();
        doc["state"] = "rebooting".into();
        doc["radio"]["state"] = "unknown".into();
        let ctx = nb_ctx(doc);
        assert_eq!((ctx.state_key, ctx.badge), (None, "neutral"));
        assert_eq!(ctx.radio_state.as_deref(), Some("unknown"));
        assert_eq!(ctx.radio_state_key, None);
    }
}
