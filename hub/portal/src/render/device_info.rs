//! The shared "Device information" block (`partials/device_info.html`,
//! plan/portal/metadata.cs.md §5, amendment 2026-10-15): the editable name,
//! description and location, then the static facts of one Hub, Node, Beacon
//! or wired probe.
//!
//! The builders are pure: every label is an i18n key the template
//! translates, every time is RFC 3339 for `short_dt`, so a test reads the
//! context without a template or a locale.

use serde::Serialize;

use super::{BeaconCtx, CalibrationCardCtx, DeviceMetaCtx, HubDescriptionCtx, NodeDetailCtx};

/// One `partials/device_info.html` include.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfoCtx {
    /// `hub` | `node` | `beacon` | `probe`: the badge and the element ids.
    pub kind: &'static str,
    /// The kind's existing metadata route; all three fields post together.
    pub action: String,
    /// The probe posts with htmx into `#command-status` (its name and
    /// location are a signed Hub command); the rest post and redirect.
    pub htmx: bool,
    /// The tab the redirect returns to (`overview`), `None` for the probe.
    pub tab: Option<&'static str>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    /// Manager+: the values are tinted and clickable.
    pub can_edit: bool,
    /// The identity on the right of the header (hostname, DevEUI, MAC, ROM).
    pub ident: Option<String>,
    pub facts: Vec<InfoFact>,
    /// The save's flash, drawn inside the block.
    pub ok: Option<String>,
    pub error: Option<String>,
}

/// One static fact of the grid.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct InfoFact {
    /// i18n key of the label.
    pub label: &'static str,
    /// The value as text; `None` (with no `value_key`) renders "—".
    pub value: Option<String>,
    /// An i18n key to render instead of `value`, `{n}` replaced by `value_n`.
    pub value_key: Option<String>,
    pub value_n: Option<i64>,
    /// `value` is RFC 3339, rendered through `short_dt`.
    pub dt: bool,
    pub mono: bool,
    /// A copy button (DevEUI, MAC, serial number only).
    pub copy: bool,
    /// A muted i18n hint after the value ("by signal").
    pub hint: Option<&'static str>,
    /// `valid` | `warning` | `expired` | `none` — the calibration's colour.
    pub tone: Option<&'static str>,
}

impl InfoFact {
    fn text(label: &'static str, value: Option<String>) -> Self {
        Self {
            label,
            value: value.filter(|v| !v.trim().is_empty()),
            ..Self::default()
        }
    }
    fn mono(label: &'static str, value: Option<String>) -> Self {
        Self {
            mono: true,
            ..Self::text(label, value)
        }
    }
    fn copy(label: &'static str, value: Option<String>) -> Self {
        let mut f = Self::mono(label, value);
        f.copy = f.value.is_some();
        f
    }
    fn dt(label: &'static str, value: Option<String>) -> Self {
        Self {
            dt: true,
            ..Self::text(label, value)
        }
    }
    fn key(label: &'static str, key: String) -> Self {
        Self {
            label,
            value_key: Some(key),
            ..Self::default()
        }
    }
}

/// The calibration fact, worded like the calibration card's summary row.
fn calibration_fact(cal: Option<&CalibrationCardCtx>) -> InfoFact {
    match cal {
        Some(cal) => InfoFact {
            value_n: cal.days_left,
            tone: Some(cal.tone),
            ..InfoFact::key(L_CALIBRATION, cal.summary_key.to_string())
        },
        None => InfoFact::text(L_CALIBRATION, None),
    }
}

const L_CALIBRATION: &str = "monitoring.device_info.calibration";
const L_SERIAL: &str = "monitoring.device_info.serial";
const L_HOME_HUB: &str = "monitoring.device_info.home_hub";
const L_LAST_SEEN: &str = "monitoring.device_info.last_seen";
const L_BATTERY: &str = "monitoring.device_info.battery";

/// The editable half, shared by every kind.
struct Editable {
    name: Option<String>,
    description: Option<String>,
    location: Option<String>,
}

fn base(
    kind: &'static str,
    action: String,
    e: Editable,
    can_edit: bool,
    ident: Option<String>,
    facts: Vec<InfoFact>,
) -> DeviceInfoCtx {
    DeviceInfoCtx {
        kind,
        action,
        htmx: false,
        tab: Some("overview"),
        name: e.name,
        description: e.description,
        location: e.location,
        can_edit,
        ident,
        facts,
        ok: None,
        error: None,
    }
}

/// Hub: the union of the Overview's former "Device description" and the
/// Unit tab's metadata card.
pub fn hub_info_ctx(
    hostname: &str,
    meta: &DeviceMetaCtx,
    description: Option<&HubDescriptionCtx>,
    agent_version: Option<String>,
    uptime: Option<String>,
    can_edit: bool,
) -> DeviceInfoCtx {
    let radio = match description {
        Some(HubDescriptionCtx {
            radio_mode: Some(mode),
            ..
        }) => InfoFact::key(
            "monitoring.device_info.radio_mode",
            format!("monitoring.hubs.radio_mode_{mode}"),
        ),
        _ => InfoFact::text(
            "monitoring.device_info.radio_mode",
            description.and_then(|d| d.radio_mode_other.clone()),
        ),
    };
    let facts = vec![
        InfoFact::mono(
            "monitoring.device_info.hostname",
            Some(hostname.to_string()),
        ),
        InfoFact::copy(L_SERIAL, description.and_then(|d| d.serial.clone())),
        InfoFact::text(
            "monitoring.device_info.model",
            description.and_then(|d| d.model_revision.clone()),
        ),
        radio,
        InfoFact::mono("monitoring.device_info.agent_version", agent_version),
        InfoFact::text("monitoring.device_info.uptime", uptime),
    ];
    base(
        "hub",
        format!("/hubs/{hostname}/meta"),
        Editable {
            name: meta.name.clone(),
            description: meta.description.clone(),
            location: meta.location.clone(),
        },
        can_edit,
        Some(hostname.to_string()),
        facts,
    )
}

/// `fw_major.fw_minor.fw_patch` of the Node's info document.
fn node_firmware(n: &NodeDetailCtx) -> Option<String> {
    let info = n.info.as_ref()?;
    let part = |k: &str| {
        info.get_attr(k)
            .ok()
            .filter(|v| !v.is_undefined() && !v.is_none())
            .map(|v| v.to_string())
    };
    Some(format!(
        "{}.{}.{}",
        part("fw_major")?,
        part("fw_minor")?,
        part("fw_patch")?
    ))
}

/// Node: `home` is [`super::HomeGateways::of`], the Nodes list's Gateway
/// column.
pub fn node_info_ctx(
    n: &NodeDetailCtx,
    serial: Option<String>,
    description: Option<String>,
    home: Option<String>,
    cal: Option<&CalibrationCardCtx>,
    can_edit: bool,
) -> DeviceInfoCtx {
    let p2p = n.transport == portal_domain::Transport::P2p;
    let mut home = InfoFact::mono(L_HOME_HUB, home);
    if home.value.is_some() {
        home.hint = Some(if p2p {
            "monitoring.device_info.home_hint_p2p"
        } else {
            "monitoring.device_info.home_hint_signal"
        });
    }
    let facts = vec![
        InfoFact::copy(
            "monitoring.device_info.dev_eui",
            Some(n.node_key.to_uppercase()),
        ),
        InfoFact::copy(L_SERIAL, serial),
        InfoFact::mono("monitoring.device_info.firmware", node_firmware(n)),
        InfoFact::key(
            "monitoring.device_info.transport",
            format!("monitoring.transport.{}", n.transport.as_str()),
        ),
        home,
        InfoFact::dt("monitoring.device_info.last_join", n.last_join_at.clone()),
        InfoFact::dt(L_LAST_SEEN, n.last_seen.clone()),
        InfoFact::text(L_BATTERY, n.info_view.battery.clone()),
        calibration_fact(cal),
    ];
    base(
        "node",
        format!("/nodes/{}/meta", n.node_key),
        Editable {
            name: n.name.clone(),
            description,
            location: n.location.clone(),
        },
        can_edit,
        Some(n.node_key.to_uppercase()),
        facts,
    )
}

/// Beacon: the protocol is the read-back's, `heard by` counts the Hubs.
pub fn beacon_info_ctx(
    b: &BeaconCtx,
    cal: Option<&CalibrationCardCtx>,
    can_edit: bool,
) -> DeviceInfoCtx {
    let protocol = match b.ble_config.as_ref().and_then(|c| c.protocol) {
        Some(p) => {
            let code = serde_json::to_value(p)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            let code = if code == "unknown" {
                "other".to_string()
            } else {
                code
            };
            InfoFact {
                value_n: b.ble_config.as_ref().and_then(|c| c.protocol_type),
                ..InfoFact::key(
                    "monitoring.device_info.protocol",
                    format!("monitoring.beacon_detail.protocol_{code}"),
                )
            }
        }
        None => InfoFact::text("monitoring.device_info.protocol", None),
    };
    let heard = b.hubs.len();
    let facts = vec![
        InfoFact::copy("monitoring.device_info.mac", Some(b.mac.clone())),
        InfoFact::copy(L_SERIAL, b.serial.clone()),
        protocol,
        InfoFact::mono(L_HOME_HUB, b.home_hub.clone()),
        InfoFact {
            value_n: Some(heard as i64),
            ..InfoFact::key(
                "monitoring.device_info.heard_by",
                if heard == 1 {
                    "monitoring.device_info.heard_by_one".to_string()
                } else {
                    "monitoring.device_info.heard_by_n".to_string()
                },
            )
        },
        InfoFact::dt(L_LAST_SEEN, b.last_seen.clone()),
        InfoFact::text(L_BATTERY, b.battery.clone()),
        calibration_fact(cal),
    ];
    base(
        "beacon",
        format!("/beacons/{}/meta", b.mac),
        Editable {
            name: b.name.clone(),
            description: b.meta.description.clone(),
            location: b.location.clone(),
        },
        can_edit,
        Some(b.mac.clone()),
        facts,
    )
}

/// What a wired probe's block is built from.
pub struct ProbeInfoInputs<'a> {
    pub hostname: &'a str,
    /// The Hub's display name.
    pub hub_name: &'a str,
    /// Wire line, 0..7.
    pub line: u8,
    pub rom: Option<String>,
    pub sensor_type: Option<String>,
    pub updated_at: Option<String>,
    pub name: Option<String>,
    pub location: Option<String>,
    pub description: Option<String>,
}

/// Wired probe: name and location are the Hub's, saved as one signed
/// command (`set_sensor_meta`), so the block posts with htmx.
pub fn probe_info_ctx(
    p: ProbeInfoInputs<'_>,
    cal: Option<&CalibrationCardCtx>,
    can_edit: bool,
) -> DeviceInfoCtx {
    let facts = vec![
        InfoFact {
            value_n: Some(i64::from(p.line) + 1),
            ..InfoFact::key(
                "monitoring.device_info.line",
                "monitoring.shared.line_fallback".to_string(),
            )
        },
        InfoFact::mono("monitoring.device_info.rom", p.rom.clone()),
        InfoFact::text("monitoring.device_info.sensor_type", p.sensor_type),
        InfoFact::text("monitoring.device_info.hub", Some(p.hub_name.to_string())),
        InfoFact::dt("monitoring.device_info.updated", p.updated_at),
        calibration_fact(cal),
    ];
    let mut ctx = base(
        "probe",
        format!("/hubs/{}/sensors/{}/meta", p.hostname, p.line),
        Editable {
            name: p.name,
            description: p.description,
            location: p.location,
        },
        can_edit,
        p.rom,
        facts,
    );
    ctx.htmx = true;
    ctx.tab = None;
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> DeviceMetaCtx {
        DeviceMetaCtx {
            name: Some("Sklad".into()),
            description: Some("Rack 2".into()),
            created_at: None,
            added_by: None,
            offline_k_override: None,
            backfill_auto: false,
            serial: None,
            location: Some("Hala A".into()),
        }
    }

    fn labels(ctx: &DeviceInfoCtx) -> Vec<&'static str> {
        ctx.facts.iter().map(|f| f.label).collect()
    }

    #[test]
    fn the_hub_block_carries_the_meta_and_both_former_cards_facts() {
        let desc = HubDescriptionCtx {
            serial: Some("10000000abcd".into()),
            model_revision: Some("CM4 / 1.1".into()),
            radio_mode: Some("p2p"),
            radio_mode_other: None,
        };
        let ctx = hub_info_ctx(
            "hub1",
            &meta(),
            Some(&desc),
            Some("2.4.0".into()),
            Some("3 d 4 h".into()),
            true,
        );
        assert_eq!(ctx.kind, "hub");
        assert_eq!(ctx.action, "/hubs/hub1/meta");
        assert_eq!(ctx.tab, Some("overview"));
        assert!(!ctx.htmx);
        assert_eq!(ctx.name.as_deref(), Some("Sklad"));
        assert_eq!(ctx.location.as_deref(), Some("Hala A"));
        assert_eq!(
            labels(&ctx),
            [
                "monitoring.device_info.hostname",
                L_SERIAL,
                "monitoring.device_info.model",
                "monitoring.device_info.radio_mode",
                "monitoring.device_info.agent_version",
                "monitoring.device_info.uptime",
            ]
        );
        // Copy on the serial only — the hostname is the header's identity.
        let copies: Vec<_> = ctx
            .facts
            .iter()
            .filter(|f| f.copy)
            .map(|f| f.label)
            .collect();
        assert_eq!(copies, [L_SERIAL]);
        assert_eq!(
            ctx.facts[3].value_key.as_deref(),
            Some("monitoring.hubs.radio_mode_p2p")
        );
    }

    #[test]
    fn a_hub_that_reported_nothing_renders_dashes_not_copy_buttons() {
        let ctx = hub_info_ctx("hub1", &DeviceMetaCtx::default(), None, None, None, false);
        assert!(!ctx.can_edit);
        assert!(ctx.facts.iter().all(|f| !f.copy));
        assert!(ctx.facts[1].value.is_none());
        assert!(ctx.facts[3].value_key.is_none() && ctx.facts[3].value.is_none());
    }

    #[test]
    fn the_calibration_fact_is_the_card_summary() {
        assert_eq!(calibration_fact(None).value_key, None);
    }

    #[test]
    fn the_probe_block_posts_one_htmx_request_and_names_its_line() {
        let ctx = probe_info_ctx(
            ProbeInfoInputs {
                hostname: "hub1",
                hub_name: "Sklad",
                line: 1,
                rom: Some("28FF4C1A62160367".into()),
                sensor_type: Some("DS18B20".into()),
                updated_at: None,
                name: Some("Inlet".into()),
                location: None,
                description: Some("pipe".into()),
            },
            None,
            true,
        );
        assert_eq!(ctx.action, "/hubs/hub1/sensors/1/meta");
        assert!(ctx.htmx);
        assert_eq!(ctx.tab, None);
        assert_eq!(ctx.facts[0].value_n, Some(2));
        assert_eq!(ctx.ident.as_deref(), Some("28FF4C1A62160367"));
        assert!(
            ctx.facts.iter().all(|f| !f.copy),
            "no copy button on a probe"
        );
        assert_eq!(ctx.facts[1].label, "monitoring.device_info.rom");
    }
}
