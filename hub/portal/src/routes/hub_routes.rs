//! The Hub (FIBER) route family — `GET /hubs`, the four-tab detail
//! `GET /hubs/{hub}/{overview|sensors|unit|system}` and every Hub mutation
//! under `/hubs/{hub}/…` (plan/portal/devices.hub.md §2, §3, §8.1).
//!
//! It replaces the single-hub `/device` page and `POST /sensors/{line}/
//! threshold`: `{hub}` is now part of every path, so a command is published
//! to the hostname the URL names instead of to whichever device happened to
//! be first in the store (§8.2 rule 2). `/device` stays as a 303 for
//! bookmarks and for the old `?tab=configuration` link.
//!
//! Context contracts: `templates/pages/hubs.html` and
//! `templates/pages/hub_detail.html` header comments, plus
//! `templates/partials/command_status.html` for the command progress shell.
//!
//! Flash messages: `?err=<code>`/`?ok=<code>` on the pairing redirect —
//! short slugs mapped to full sentences in [`err_message`]/[`ok_message`]
//! rather than putting free text in the URL (no percent-encoding of a
//! `Location` header we build ourselves, no `urlencoding` dependency).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use minijinja::context;
use serde::Deserialize;
use serde_json::json;

use crate::auth::{CurrentUser, RequireAdmin, RequireManager};
use crate::http::AppState;
use crate::relay::{CommandOutcome, RelayError};
use crate::render;
use crate::state::PortalState;

use super::command::Cmd;
use super::{non_empty, render as render_page, render_fragment, tr};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/hubs", get(hubs_page))
        .route("/hubs/add", get(hub_add_page))
        .route("/hubs/{hub}", get(hub_index))
        .route("/hubs/{hub}/{tab}", get(hub_detail))
        .route("/hubs/{hub}/radio-card", get(radio_card))
        .route("/hubs/{hub}/sb-firmware", get(sb_firmware_card))
        .route("/hubs/{hub}/nb-firmware", get(nb_firmware_card))
        .route("/hubs/{hub}/radio/gateways", post(add_gateway))
        .route("/hubs/{hub}/radio/gateways/remove", post(remove_gateway))
        .route("/hubs/{hub}/meta", post(set_hub_meta))
        .route("/hubs/{hub}/pair", post(pair_device))
        .route("/hubs/{hub}/unpair", post(unpair_device))
        .route("/hubs/{hub}/system/restart", post(restart))
        .route("/hubs/{hub}/system/power-off", post(power_off))
        .route("/hubs/{hub}/system/factory-reset", post(factory_reset))
        .route("/hubs/{hub}/system/radio-mode", post(set_radio_mode))
        .route(
            "/hubs/{hub}/system/southbridge-update",
            post(southbridge_update),
        )
        .route("/hubs/{hub}/radio/move-nodes", post(move_all_nodes))
        .route(
            "/hubs/{hub}/radio/northbridge-update",
            post(northbridge_update),
        )
        .route("/hubs/{hub}/sensors/{line}/threshold", post(set_threshold))
        .route("/hubs/{hub}/sensors/{line}/row", get(sensor_row))
        .route("/hubs/{hub}/sensors/{line}/name", post(set_sensor_name))
        .route("/hubs/{hub}/sensors/{line}/meta", post(set_probe_meta))
        .route(
            "/hubs/{hub}/sensors/{line}/location",
            post(set_sensor_location),
        )
        .route(
            "/hubs/{hub}/system/info-interval",
            post(set_system_info_interval),
        )
        .route("/hubs/{hub}/system/read-interval", post(set_read_interval))
        .route("/hubs/{hub}/display/device-label", post(set_device_label))
        .route("/hubs/{hub}/display/sensors", post(set_display_sensors))
        .route("/hubs/{hub}/display/picker", get(display_picker))
        .route("/hubs/{hub}/display/{setting}", post(set_display_scalar))
        .route("/device", get(legacy_device_page))
}

/// The four tabs of the detail page (§3). Anything else in that path
/// segment is a 404, like any other address the portal does not serve.
const TABS: [&str; 4] = ["overview", "sensors", "unit", "system"];

// ---------------------------------------------------------------------
// Which Hubs this portal serves
// ---------------------------------------------------------------------

/// The one place that decides whether `{hub}` is a Hub this portal knows
/// (§8.2 rule 1): the state store **∪ the Client registry** (§12 C, J). A
/// registered Client that has never connected has no `DeviceState`, and it
/// still has a detail page — that is the registry's point (§1.2).
///
/// Returns the stored spelling of the hostname, so a case-insensitive URL
/// still addresses the right device (lowercase since decision #21; a URL may
/// not be). `None` means 404 — the same answer as any unserved path, never
/// "this hub does not exist".
pub(super) fn known_hub(state: &AppState, hub: &str) -> Option<String> {
    known_hub_in(&state.store.snapshot(), &state.cluster, hub)
}

/// [`known_hub`] against a snapshot the caller already holds, so a handler
/// that goes on to read the device does not clone the state twice.
fn known_hub_in(
    portal_state: &PortalState,
    registry: &crate::cluster::Registry,
    hub: &str,
) -> Option<String> {
    let wanted = hub.trim();
    if wanted.is_empty() {
        return None;
    }
    portal_state
        .devices
        .values()
        .find(|d| d.hostname.trim().eq_ignore_ascii_case(wanted))
        .map(|d| d.hostname.clone())
        .or_else(|| registry.get(wanted).map(|row| row.hostname))
}

/// The Hub this portal is scoped to when no `{hub}` is in the path — the
/// Server. Phase-1 "first device in the store", as `/device` has always
/// resolved it.
fn current_hostname(state: &AppState) -> Option<String> {
    state
        .store
        .snapshot()
        .devices
        .values()
        .next()
        .map(|d| d.hostname.clone())
}

/// One Hub's `device_meta` row (devices.hub.md §3.1, §12 D): `scope =
/// 'hub'`, `entity =` the hostname, because a Hub is its own entity — the
/// Node and Beacon rows key on a `node_key` or a MAC below the hostname.
///
/// A read failure is `None`, the same answer as "never edited": the page it
/// feeds renders the Hub either way, and refusing to draw the device
/// because its optional description could not be loaded would be the wrong
/// trade.
pub(crate) async fn hub_meta(
    state: &AppState,
    hostname: &str,
) -> Option<portal_storage::DeviceMeta> {
    match ({
        let a0 = hostname.to_string();
        let a1 = portal_storage::THRESHOLD_SCOPE_HUB.to_string();
        let a2 = hostname.to_string();
        state.db.run(move |s| s.get_device_meta(&a0, &a1, &a2))
    })
    .await
    {
        Ok(row) => row,
        Err(err) => {
            tracing::warn!(%err, hostname, "failed to read hub metadata");
            None
        }
    }
}

/// The generic not-found page, rendered for an unknown `{hub}` exactly as
/// for an unknown path: no "does this hub exist" oracle (§8.2 rule 1).
use super::not_found;

// ---------------------------------------------------------------------
// GET /hubs
// ---------------------------------------------------------------------

/// `GET /hubs/add` — Add hub as a page, like `/beacons/add` (devices.hub.md,
/// dodatek 2026-10-14). Admin only and only where there is a Server to add
/// to (§2.6, §9): anyone else is sent back to the list, where the button is
/// not rendered either. The POSTs behind the form stay `RequireAdmin`.
async fn hub_add_page(current_user: CurrentUser, State(state): State<AppState>) -> Response {
    let server = match (
        current_user.is_admin(),
        super::cluster_routes::local_server(&state),
    ) {
        (true, Some(server)) => server,
        _ => return Redirect::to("/hubs").into_response(),
    };
    let add_hub = super::cluster_routes::add_hub_ctx(&state, &server).await;
    let ctx = render::page_ctx(
        context! { nav_active => "hubs", add_hub => add_hub },
        Some(&current_user),
    );
    render_page(&state, "pages/hub_add.html", ctx)
}

async fn hubs_page(current_user: CurrentUser, State(state): State<AppState>) -> Response {
    let portal_state = state.store.snapshot();
    let registry = state.cluster.list();
    // One `device_meta` read per row rather than one for the page: the row
    // is keyed `(hostname, 'hub', hostname)`, so every Hub is its own
    // hostname bucket and `list_device_meta` could not fetch them together.
    // Safe at this size — §2.7 puts the list in units of rows, never
    // hundreds, and the Node list's bulk read exists for the opposite case.
    //
    // Order (§2.2): the Server first, then the registered Clients from the
    // newest registration, then any other Hub the store holds (OQ 18).
    let mut hubs: Vec<render::HubRowCtx> =
        Vec::with_capacity(portal_state.devices.len() + registry.len());
    let mut store_rows: Vec<render::HubRowCtx> = Vec::new();
    let mut server_row: Option<render::HubRowCtx> = None;
    for device in portal_state.devices.values() {
        if state.cluster.get(&device.hostname).is_some() {
            continue;
        }
        let meta = hub_meta(&state, &device.hostname).await;
        let mut row =
            render::hub_row_ctx_with_meta(device, meta.as_ref(), state.alarms.hub_faults());
        row.radio_control_off = radio_control_off(&state, &device.hostname);
        row.unpaired = matches!(state.keys.device_identity(&device.hostname).await, Ok(None));
        if server_row.is_none() && is_local_device(&device.hostname, &state.local_hostname) {
            row.kind = Some("server");
            server_row = Some(row);
        } else {
            if reports_client_role(device) {
                row.kind = Some("client");
            }
            store_rows.push(row);
        }
    }
    hubs.extend(server_row);
    // A Client joined outside this portal (the agent's own `client-add`) has no registry row,
    // but it says what it is: those go first among the other Hubs.
    store_rows.sort_by_key(|row| row.kind != Some("client"));
    let server_mode = state.radio.mode().map(|m| m.as_str());
    for client in &registry {
        let meta = hub_meta(&state, &client.hostname).await;
        let device = portal_state.device(&client.hostname);
        let cluster_row = super::cluster_routes::client_row_ctx(client, device, server_mode);
        let row = match device {
            Some(device) => {
                let mut row =
                    render::hub_row_ctx_with_meta(device, meta.as_ref(), state.alarms.hub_faults());
                row.unpaired =
                    matches!(state.keys.device_identity(&device.hostname).await, Ok(None));
                row.gateway_eui = row.gateway_eui.or_else(|| client.gateway_eui.clone());
                row.kind = Some("client");
                row.cluster = Some(cluster_row);
                row
            }
            None => render::registered_hub_row_ctx(cluster_row, meta.as_ref()),
        };
        hubs.push(row);
    }
    hubs.extend(store_rows);
    for row in &mut hubs {
        row.calibration = state
            .calibration
            .worst_probe_level(&row.hostname)
            .map(|l| l.as_str());
    }

    // §2.2: Typ renders only when the list really holds both kinds.
    let show_type = !registry.is_empty() || hubs.iter().any(|row| row.kind == Some("client"));
    // §2.6: admin+, and only where there is a Server to add to (§9 first row).
    let add_hub = match (
        current_user.is_admin(),
        super::cluster_routes::local_server(&state),
    ) {
        (true, Some(server)) => Some(super::cluster_routes::add_hub_ctx(&state, &server).await),
        _ => None,
    };

    let ctx = render::page_ctx(
        context! {
            nav_active => "hubs",
            hubs => hubs,
            show_type => show_type,
            add_hub => add_hub,
            is_admin => current_user.is_admin(),
        },
        Some(&current_user),
    );
    render_page(&state, "pages/hubs.html", ctx)
}

// ---------------------------------------------------------------------
// GET /hubs/{hub} and GET /hubs/{hub}/{tab}
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct FlashQuery {
    err: Option<String>,
    ok: Option<String>,
}

fn ok_message(code: &str) -> Option<String> {
    let key = match code {
        "paired" => "monitoring.device.ok_paired",
        "unpaired" => "monitoring.device.ok_unpaired",
        // devices.hub.md §3.1 — the Hub's name and description.
        "meta_saved" => "monitoring.meta.saved",
        _ => return None,
    };
    Some(tr(key))
}

fn err_message(code: &str) -> Option<String> {
    let key = match code {
        "invalid_code" => "monitoring.device.err_invalid_code",
        "no_device" => "monitoring.shared.no_device_yet",
        "timeout" => "monitoring.device.err_pair_timeout",
        "pair_failed" => "monitoring.device.err_pair_failed",
        "bad_code" => "monitoring.device.err_bad_code",
        "not_pairing_mode" => "monitoring.device.err_not_pairing_mode",
        "local_decrypt" => "monitoring.device.err_local_decrypt",
        "mismatch" => "monitoring.device.err_mismatch",
        "store_failed" => "monitoring.device.err_store_failed",
        "unpair_failed" => "monitoring.device.err_unpair_failed",
        "meta_save_failed" => "monitoring.meta.err_save_failed",
        "description_too_long" => "monitoring.meta.err_description_too_long",
        _ => "monitoring.shared.something_went_wrong",
    };
    Some(tr(key))
}

/// `/hubs/{hub}` without a tab — 303 to Overview (§3).
async fn hub_index(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path(hub): Path<String>,
) -> Response {
    match known_hub(&state, &hub) {
        Some(hostname) => Redirect::to(&format!("/hubs/{hostname}/overview")).into_response(),
        None => not_found(&state, Some(&current_user)),
    }
}

/// `/device` — the address the portal shipped with, kept as a redirect
/// (§3, decision 4). The old `?tab=configuration` deep link lands on the
/// System tab, which is where its cards moved.
#[derive(Deserialize)]
struct LegacyTabQuery {
    tab: Option<String>,
}

async fn legacy_device_page(
    _current_user: CurrentUser,
    State(state): State<AppState>,
    Query(q): Query<LegacyTabQuery>,
) -> Response {
    let Some(hostname) = current_hostname(&state) else {
        // No Hub has reported in: the list page is what renders the
        // waiting state (§2.5), so there is nothing to scope a detail to.
        return Redirect::to("/hubs").into_response();
    };
    let tab = match q.tab.as_deref() {
        Some("configuration") => "system",
        _ => "overview",
    };
    Redirect::to(&format!("/hubs/{hostname}/{tab}")).into_response()
}

async fn hub_detail(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path((hub, tab)): Path<(String, String)>,
    Query(q): Query<FlashQuery>,
) -> Response {
    if !TABS.contains(&tab.as_str()) {
        return not_found(&state, Some(&current_user));
    }
    let portal_state = state.store.snapshot();
    let Some(hostname) = known_hub_in(&portal_state, &state.cluster, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    let device = portal_state.device(&hostname);

    let pairing = match state.keys.device_identity(&hostname).await {
        Ok(info) => info,
        Err(err) => {
            tracing::error!(%err, hostname, "failed to load device identity");
            None
        }
    };

    let paired_by_username = match pairing.as_ref().and_then(|p| p.paired_by_user_id) {
        Some(id) => match state.auth.list_users(true).await {
            // The display name like everywhere else a person is named.
            Ok(users) => users.into_iter().find(|u| u.id == id).map(|u| {
                let names: std::collections::HashMap<String, String> = u
                    .full_name
                    .clone()
                    .filter(|n| !n.trim().is_empty())
                    .map(|n| (u.username.clone(), n))
                    .into_iter()
                    .collect();
                render::person_label(&u.username, &names)
            }),
            Err(_) => None,
        },
        None => None,
    };

    // Only worth a query when there is a pairing with no user on it — see
    // `pairing_came_from_an_invite`.
    let paired_via_invite = pairing.is_some()
        && paired_by_username.is_none()
        && pairing_came_from_an_invite(&state, &hostname).await;

    let error = q.err.and_then(non_empty).and_then(|c| err_message(&c));
    let ok = q.ok.and_then(non_empty).and_then(|c| ok_message(&c));
    let local_device = is_local_device(&hostname, &state.local_hostname);
    let can_edit = render::can_edit_thresholds(&current_user);
    let meta = hub_meta(&state, &hostname).await;
    // UX-059: the overview says when this Hub's alarm e-mails reach nobody,
    // neither through its own row nor the default recipients (#31).
    let no_alarm_recipients = tab == "overview" && {
        let h = hostname.clone();
        let (cfg, defaults) = state
            .db
            .run(move |s| {
                Ok((
                    s.get_device_report_config(&h)?,
                    s.default_alarm_recipients()?,
                ))
            })
            .await
            .unwrap_or_default();
        !crate::alarms::notifier::has_alarm_recipients(cfg.as_ref(), &defaults)
    };

    // C5 (§1.2, §3.4): a registered Client's facts, and on the Server's
    // System tab the cluster card with its Clients and Add hub.
    let server_mode = state.radio.mode().map(|m| m.as_str());
    let client = state
        .cluster
        .get(&hostname)
        .map(|row| super::cluster_routes::client_row_ctx(&row, device, server_mode));
    let cluster_card = if tab == "system" && client.is_none() && local_device {
        let mut clients = Vec::new();
        for row in state.cluster.list() {
            let d = portal_state.device(&row.hostname);
            let name = super::cluster_routes::client_name(&state, &row.hostname).await;
            clients.push(context! {
                name => name,
                ..minijinja::Value::from_serialize(super::cluster_routes::client_row_ctx(&row, d, server_mode))
            });
        }
        let add_hub = if current_user.is_admin() {
            Some(super::cluster_routes::add_hub_ctx(&state, &hostname).await)
        } else {
            None
        };
        Some(context! { clients => clients, add_hub => add_hub })
    } else {
        None
    };
    let kind = if client.is_some() || device.is_some_and(reports_client_role) {
        Some("client")
    } else if local_device {
        Some("server")
    } else {
        None
    };

    // The Sensors tab's calibration icons (calibration.cs.md, UI amendment
    // 2026-10-14); the card itself is on each probe's page.
    let mut device_ctx = device.map(|d| render::device_status_ctx(d, can_edit));
    if let (Some(ctx), Some(d), true) = (device_ctx.as_mut(), device, tab == "sensors") {
        set_probe_calibration_flags(&state, &d.hostname, &mut ctx.sensors);
        // Lines 1-8 ascending: the default order, whatever order the Hub
        // reported them in.
        ctx.sensors.sort_by_key(|s| s.line);
    }
    // "Device information" (metadata.cs.md §5): the Overview's one block of
    // name, description, location and the Hub's facts.
    let device_info = (tab == "overview").then(|| {
        let meta_ctx = render::device_meta_ctx(meta.as_ref());
        let description = device.map(render::hub_description_ctx);
        render::hub_info_ctx(
            &hostname,
            &meta_ctx,
            description.as_ref(),
            device_ctx.as_ref().and_then(|d| d.display_version.clone()),
            device.and_then(|d| {
                render::system_status_ctx(d, &portal_state.mqtt)
                    .system
                    .uptime
            }),
            can_edit,
        )
    });
    // The Unit tab's Alarm messages card (ui_components.md, amendment
    // 2026-10-15): this Hub's own `sys:*` alarms.
    let diag_alarms = if tab == "unit" {
        let filter = portal_storage::AlarmListFilter {
            limit: 50,
            hostname: Some(hostname.clone()),
            sensor_id_prefix: Some("sys:".to_string()),
            ..Default::default()
        };
        let rows = state
            .db
            .run(move |s| s.list_alarms(&filter))
            .await
            .map(|(rows, _)| rows)
            .unwrap_or_else(|err| {
                tracing::warn!(%err, hostname, "failed to load the Hub's alarms");
                Vec::new()
            });
        Some(render::diag_alarm_events(&rows, 50))
    } else {
        None
    };
    let ctx = render::page_ctx(
        context! {
            nav_active => "hubs",
            active_tab => tab,
            diag_alarms => diag_alarms,
            di => device_info,
            // Explicit, because `device_page_ctx` takes it from the store and
            // a registered Client that has never connected has no entry there.
            hostname => hostname.clone(),
            client => client,
            cluster_card => cluster_card,
            hub_kind => kind,
            // Overrides `device_page_ctx`'s `device_label` (explicit keys
            // win over `..base` in `context!`): the header shows the
            // operator's own name first — devices.hub.md §3.1, and the one
            // name a factory reset cannot wipe.
            device_label => render::hub_display_name(device, meta.as_ref()),
            meta => render::device_meta_ctx(meta.as_ref()),
            can_edit_meta => can_edit,
            locations => if tab == "overview" { super::location_suggestions(&state).await } else { Vec::new() },
            no_alarm_recipients => no_alarm_recipients,
            error => error,
            ok => ok,
            device => device_ctx,
            status => device.map(|d| render::system_status_ctx(d, &portal_state.mqtt)),
            description => device.map(render::hub_description_ctx),
            // The System tab's "Firmware SB" card (southbridge_update.md §3.7).
            sb_firmware => device.map(|d| render::sb_firmware_ctx(
                d,
                current_user.is_admin(),
                None,
                unix_now(),
            )),
            radio => device.map(|d| radio_card_ctx(
                &state,
                d,
                portal_state.radio_gateways.as_ref(),
                &current_user,
                None,
            )),
            system_info_interval_s => device.and_then(|d| d.config.system_info_interval_s),
            // The 1-Wire read interval the Hub reports (config_sync.md §13 Q18).
            read_interval_s => device
                .and_then(|d| d.config.intervals.as_ref()?.sample_interval_ms)
                .map(|ms| ms / 1000),
            // The rendered `radio.mode` of the Hub this Portal runs on, the one
            // "Změnit rádio Hubu" acts on (§3.4, UI-24 as amended 2026-09-27).
            // Unknown for any other Hub.
            radio_mode => local_device
                .then(|| state.radio.mode())
                .flatten()
                .map(|m| m.as_str()),
            // #470 H4: the P2P modulation, read-only (`lora` until P5). Like `radio_mode`, the
            // local Hub's rendered config only, and only when it runs P2P.
            p2p_modulation => local_device
                .then(|| state.radio.p2p_modulation())
                .flatten(),
            // "Přesunout všechny Nody na <rádio>" (node_registry.md §11; devices.hub.md,
            // dodatek 2026-09-27, R6): offered once the Hub runs one carrier and the registry
            // keeps Nodes on the other. The local Hub's registry only.
            move_nodes => local_device
                .then(|| state.radio.bulk_move())
                .flatten()
                .map(|plan| context! {
                    from => plan.from.as_str(),
                    to => plan.to.as_str(),
                    count => plan.dev_euis.len(),
                }),
            // "Změnit rádio Hubu" asks about the Nodes (node_registry.md §12): per mode the
            // dialog can pick, the Nodes it would leave on a carrier it switches off, and
            // where a move would take them. A mode absent here asks nothing.
            radio_switch => if local_device {
                radio_switch_ctx(&state.radio)
            } else {
                Vec::new()
            },
            is_admin => current_user.is_admin(),
            // The Unit tab (§3.3, display-config.md §4/§5). `can_manage`
            // is the gate the display controls hang off: manager+, one
            // tier below the power actions beside them.
            can_manage => can_edit,
            display_config => device.map(|d| render::display_config_ctx(&d.config)),
            sensor_catalog => device.map(crate::display_catalog::catalog),
            max_selected => crate::display_catalog::MAX_SELECTED,
            ..render::device_page_ctx(
                device,
                pairing.as_ref(),
                paired_by_username,
                paired_via_invite,
                local_device,
            ),
        },
        Some(&current_user),
    );
    render_page(&state, "pages/hub_detail.html", ctx)
}

/// The Node question of "Změnit rádio Hubu", per target mode (node_registry.md §12): only the
/// modes that switch off a carrier this Hub's registry holds Nodes on. `from` names those
/// carriers (`from_label` in the reader's language), `count` the Nodes, `move_to` the carrier a
/// move goes to (none for `off`).
fn radio_switch_ctx(radio: &crate::radio::RadioControl) -> Vec<minijinja::Value> {
    control_radio::Mode::ALL
        .into_iter()
        .filter_map(|mode| {
            let switch = radio.node_switch(mode)?;
            let mut from: Vec<&str> = switch.nodes.iter().map(|(_, c)| c.as_str()).collect();
            from.sort_unstable();
            from.dedup();
            let from_label = from
                .iter()
                .map(|c| tr(&format!("monitoring.transport.{c}")))
                .collect::<Vec<_>>()
                .join(", ");
            Some(context! {
                mode => mode.as_str(),
                count => switch.nodes.len(),
                from => from,
                from_label => from_label,
                move_to => switch.move_to.map(|c| c.as_str()),
            })
        })
        .collect()
}

/// The card's own refetch query while it follows a manual Northbridge update
/// ([`render::nb_update_progress`]): `?nbu=<wait|run|done>&since=<unix>`.
#[derive(Deserialize, Default)]
struct RadioCardQuery {
    nbu: Option<String>,
    since: Option<i64>,
}

impl RadioCardQuery {
    fn progress(&self) -> Option<(render::NbUpdatePhase, i64)> {
        let phase = render::NbUpdatePhase::parse(self.nbu.as_deref()?)?;
        Some((phase, self.since?))
    }
}

/// `GET /hubs/{hub}/radio-card`: the "Radio & gateways" card alone, fetched by
/// the Overview tab on the `radio-{hostname}` / `radio-gateways` SSE nudges
/// (devices.hub.md §6). Id-only nudges, so every open tab refetches the card
/// rather than the server rendering one copy per viewer. While the card follows
/// a manual Northbridge update it also polls itself, carrying the update's
/// phase in its query ([`RadioCardQuery`]).
async fn radio_card(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Query(q): Query<RadioCardQuery>,
) -> Response {
    let portal_state = state.store.snapshot();
    let Some(hostname) = known_hub_in(&portal_state, &state.cluster, &hub) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(device) = portal_state.device(&hostname) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    render_fragment(
        &state,
        "partials/hub_radio_card.html",
        context! { radio => radio_card_ctx(
            &state,
            device,
            portal_state.radio_gateways.as_ref(),
            &current_user,
            q.progress(),
        ) },
    )
}

/// `GET /hubs/{hub}/nb-firmware`: the System tab's "Northbridge firmware" card alone
/// (ui_components.md §13, 2026-10-15), refetched on the `radio-{hostname}` nudge and,
/// while it follows a manual update, every 3 s with the phase in its query.
async fn nb_firmware_card(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Query(q): Query<RadioCardQuery>,
) -> Response {
    let portal_state = state.store.snapshot();
    let Some(hostname) = known_hub_in(&portal_state, &state.cluster, &hub) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(device) = portal_state.device(&hostname) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    render_fragment(
        &state,
        "partials/hub_nb_firmware.html",
        context! { radio => radio_card_ctx(
            &state,
            device,
            portal_state.radio_gateways.as_ref(),
            &current_user,
            q.progress(),
        ) },
    )
}

/// [`render::radio_card_ctx`] with [`radio_control_off`], the viewer's right to
/// „Aktualizovat firmware NB“ and a followed update applied — the one builder the
/// Overview tab, its SSE refetch and the update's own answer use.
fn radio_card_ctx(
    state: &AppState,
    device: &crate::state::DeviceState,
    gateways: Option<&crate::state::RadioGatewaysState>,
    user: &CurrentUser,
    progress: Option<(render::NbUpdatePhase, i64)>,
) -> render::RadioCardCtx {
    let base = render::radio_card_ctx(device, gateways, &client_gateways(state, device));
    let nb_progress = progress.and_then(|(phase, since)| {
        render::nb_update_progress(base.nb_firmware.as_ref(), phase, since, unix_now())
    });
    render::RadioCardCtx {
        radio_control_off: radio_control_off(state, &device.hostname),
        nb_update_allowed: user.is_admin(),
        nb_progress,
        client: state.cluster.get(&device.hostname).is_some() || reports_client_role(device),
        ..base
    }
}

/// The cluster Clients' gateways the Server's card lists beside its own (UI-26): the EUI on
/// the registry row, and the one the Client reports, each with the Client's hostname. Empty
/// on any Hub but the one this portal runs on.
fn client_gateways(state: &AppState, device: &crate::state::DeviceState) -> Vec<(String, String)> {
    if !is_local_device(&device.hostname, &state.local_hostname) {
        return Vec::new();
    }
    let snapshot = state.store.snapshot();
    let mut out = Vec::new();
    for row in state.cluster.list() {
        let reported = snapshot
            .devices
            .get(&row.hostname)
            .and_then(|d| d.lorawan.as_ref())
            .and_then(|l| l.gateway_eui.clone());
        for eui in row.gateway_eui.clone().into_iter().chain(reported) {
            out.push((eui.to_ascii_lowercase(), row.hostname.clone()));
        }
    }
    out
}

/// The "Firmware SB" card's own refetch query while it follows a manual Southbridge
/// update ([`render::sb_update_progress`]): `?sbu=<wait|run|done>&since=<unix>`. Its own
/// key, so it never mixes with the radio card's `nbu`.
#[derive(Deserialize, Default)]
struct SbFirmwareQuery {
    sbu: Option<String>,
    since: Option<i64>,
}

impl SbFirmwareQuery {
    fn progress(&self) -> Option<(render::NbUpdatePhase, i64)> {
        let phase = render::NbUpdatePhase::parse(self.sbu.as_deref()?)?;
        Some((phase, self.since?))
    }
}

/// `GET /hubs/{hub}/sb-firmware`: the System tab's "Firmware SB" card alone, refetched on
/// the Hub's `device-{hostname}-status` nudge (which `system/info` raises) and, while it
/// follows a manual update, every 3 s with the update's phase in its query.
async fn sb_firmware_card(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Query(q): Query<SbFirmwareQuery>,
) -> Response {
    let portal_state = state.store.snapshot();
    let Some(hostname) = known_hub_in(&portal_state, &state.cluster, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    let Some(device) = portal_state.device(&hostname) else {
        return not_found(&state, Some(&current_user));
    };
    render_fragment(
        &state,
        "partials/hub_sb_firmware.html",
        context! { sb_firmware => render::sb_firmware_ctx(
            device,
            current_user.is_admin(),
            q.progress(),
            unix_now(),
        ) },
    )
}

fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Whether the Hub pages must say that this portal cannot command the Hub's
/// Nodes: the Hub is the machine the portal runs on, and the in-process
/// radio seam is inert (`RadioControl::is_configured`). Only the local Hub
/// is flagged — the seam drives the local ChirpStack, and the MQTT-reported
/// health of any other Hub says nothing about it.
fn radio_control_off(state: &AppState, hostname: &str) -> bool {
    local_radio_control_off(hostname, &state.local_hostname, state.radio.is_configured())
}

/// [`radio_control_off`]'s decision, apart from `AppState` so it can be
/// tested.
fn local_radio_control_off(hostname: &str, portal_hostname: &str, configured: bool) -> bool {
    !configured && is_local_device(hostname, portal_hostname)
}

// ---------------------------------------------------------------------
// POST /hubs/{hub}/meta
// ---------------------------------------------------------------------

/// The Hub's name and description (devices.hub.md §3.1 "Název a popis").
///
/// Both fields are plain text and both clear on blank, because there is no
/// third state to express: a Hub either has an operator name or falls back
/// to the `device_label` it reports. Unlike the Node form's `offline_k`,
/// blank here is not "inherit" — it is "no name", and the reader's
/// fallback chain does the inheriting.
#[derive(Deserialize)]
struct HubMetaForm {
    name: String,
    description: String,
    #[serde(default)]
    location: String,
    /// The tab the redirect returns to: `overview` (Device information)
    /// unless the form says `unit`.
    #[serde(default)]
    tab: Option<String>,
}

/// Manager+ (§7), audited. Plain POST + redirect rather than htmx into
/// `#command-status`: that shell renders a relay `CommandOutcome`, and this
/// write never leaves SQLite — no command reaches the device, which is the
/// whole point of holding the name portal-side (§3.1).
async fn set_hub_meta(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<HubMetaForm>,
) -> Response {
    // Unknown `{hub}` is a 404 before anything is written, like every other
    // route in the family (§8.2 rule 1). `device_meta` is keyed by
    // `(hostname, scope, entity)`, so a row written for a hostname the
    // portal does not serve would be silently inherited by whatever Hub
    // claims that name next.
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    let back = match form.tab.as_deref() {
        Some("unit") => "unit",
        _ => "overview",
    };
    let overview = format!("/hubs/{hostname}/{back}");

    if form.description.trim().chars().count() > super::DESCRIPTION_MAX {
        return Redirect::to(&format!("{overview}?err=description_too_long")).into_response();
    }
    let name = non_empty(form.name);
    let description = non_empty(form.description.clone());
    let location = non_empty(form.location);

    let saved = super::edit_device_meta(
        &state,
        &current_user,
        &hostname,
        portal_storage::THRESHOLD_SCOPE_HUB,
        // A Hub is its own entity: there is no key below the hostname.
        &hostname,
        |row| {
            row.name = name.clone();
            row.description = description.clone();
            row.location = location.clone();
        },
    )
    .await;

    if let Err(err) = &saved {
        tracing::warn!(%err, hostname, "failed to save hub metadata");
    }

    super::audit_action(
        &state,
        &current_user,
        "hub_meta_set",
        &hostname,
        json!({ "name": name, "description": description, "location": location }),
        saved.is_ok(),
    )
    .await;

    if saved.is_ok() {
        Redirect::to(&format!("{overview}?ok=meta_saved")).into_response()
    } else {
        Redirect::to(&format!("{overview}?err=meta_save_failed")).into_response()
    }
}

// ---------------------------------------------------------------------
// POST /hubs/{hub}/pair and /unpair
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct PairForm {
    /// Absent for local (server) pairing — the form on the System tab has
    /// no code input at all in that case (`local_device` in
    /// `render::device_page_ctx`).
    pairing_code: Option<String>,
    /// The page whose unpaired banner posted this (#109, dodatek 2026-10-13): a successful
    /// pairing goes back there, so the action the banner blocked can be done straight away.
    /// Absent from the System tab's own card. Only a same-site path is honoured.
    return_to: Option<String>,
}

/// `return_to` if it is a path on this portal, never a URL elsewhere: a leading single `/`, no
/// scheme-relative `//`, no backslash (browsers read `/\host` as `//host`) and no control
/// characters.
fn safe_return_to(return_to: Option<&str>) -> Option<&str> {
    let path = return_to?.trim();
    let ok = path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && !path.chars().any(char::is_control);
    ok.then_some(path)
}

/// Whether `device` reports itself a Client (`system/info.setup.role`): how the portal knows a
/// Client it has no registry row for, one joined through the agent's own `client-add`.
pub(super) fn reports_client_role(device: &crate::state::DeviceState) -> bool {
    device
        .system
        .setup_role
        .as_deref()
        .is_some_and(|role| role.trim().eq_ignore_ascii_case("client"))
}

/// `true` if the device on the bus is the machine this portal runs on — in
/// which case pairing needs no typed code, because there is no second
/// machine and nobody to read the LCD to.
///
/// Comparison is trimmed and ASCII-case-insensitive: both sides are lowercase
/// since decision #21, but a Hub on an older agent still reports its
/// uppercased MQTT identity. Hostnames are ASCII by definition, so
/// `eq_ignore_ascii_case` is the whole story.
///
/// A blank `portal_hostname` (hostname unreadable, or a device that hasn't
/// reported one) is never local — the safe direction, since the fallback is
/// the existing code flow.
pub(super) fn is_local_device(device_hostname: &str, portal_hostname: &str) -> bool {
    let device = device_hostname.trim();
    let portal = portal_hostname.trim();
    !device.is_empty() && !portal.is_empty() && device.eq_ignore_ascii_case(portal)
}

/// `true` if `seed`'s derived public key matches the device's reported
/// `ca_public_key` — extracted as a pure, directly-testable seam for the
/// mismatch-refusal policy (stricter than v1, which only logged this;
/// `pairing.rs`'s own module doc calls that comparison "the caller's
/// policy call, not this function's").
fn pubkey_matches(seed: &[u8; 32], ca_public_key_hex: &str) -> bool {
    fiber_protocol::derive_public_hex(seed) == ca_public_key_hex
}

/// Where a pairing mutation sends the operator back to: the System tab it
/// was posted from, carrying its flash slug.
fn back_to_system(hostname: &str, param: &str, slug: &str) -> Response {
    Redirect::to(&format!("/hubs/{hostname}/system?{param}={slug}")).into_response()
}

#[derive(Deserialize)]
struct GatewayForm {
    #[serde(default)]
    gateway_eui: String,
    #[serde(default)]
    name: String,
}

/// `POST /hubs/{hub}/radio/gateways`: register an external LoRaWAN gateway in this Hub's
/// ChirpStack through the radio seam (control.radio `gateway-add`). Backend preparation only
/// (Hynek 2026-09-26): no page links here yet, and it is **not tested on hardware**. Admin, and
/// only the Portal's own Hub, because the seam talks to the local ChirpStack.
async fn add_gateway(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<GatewayForm>,
) -> Response {
    gateway_command(&state, &current_user, &hub, &form, false).await
}

/// `POST /hubs/{hub}/radio/gateways/remove`: control.radio `gateway-remove`, as
/// [`add_gateway`].
async fn remove_gateway(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<GatewayForm>,
) -> Response {
    gateway_command(&state, &current_user, &hub, &form, true).await
}

async fn gateway_command(
    state: &AppState,
    current_user: &CurrentUser,
    hub: &str,
    form: &GatewayForm,
    remove: bool,
) -> Response {
    let Some(hostname) = known_hub(state, hub) else {
        return not_found(state, Some(current_user));
    };
    let verb = if remove {
        "gateway-remove"
    } else {
        "gateway-add"
    };
    let cmd = Cmd::new(state, current_user, verb);
    if !is_local_device(&hostname, &state.local_hostname) {
        return cmd.fail(&tr("monitoring.hubs.gateway_not_local"));
    }
    let eui = form.gateway_eui.trim().to_ascii_lowercase();
    if eui.len() != 16 || !eui.chars().all(|c| c.is_ascii_hexdigit()) {
        return cmd.fail(&tr("monitoring.hubs.gateway_invalid_eui"));
    }
    let name = super::non_empty(form.name.trim().to_string());
    cmd.radio(
        state.radio.gateway(&eui, name.as_deref(), remove).await,
        &tr(if remove {
            "monitoring.hubs.gateway_removed"
        } else {
            "monitoring.hubs.gateway_added"
        }),
        if remove {
            "radio_gateway_remove"
        } else {
            "radio_gateway_add"
        },
        &hostname,
        json!({ "gateway_eui": eui, "name": name, "transport": "control.radio" }),
    )
    .await
}

/// Admin (§7) — the shipped route had no role gate at all while unpair was
/// admin, which the migration closes rather than carries over.
async fn pair_device(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<PairForm>,
) -> Response {
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };

    // Resolve the hostname before validating the form: whether a code is
    // required at all depends on which device we are talking to.
    let local = is_local_device(&hostname, &state.local_hostname);
    tracing::info!(
        hostname,
        portal_hostname = %state.local_hostname,
        local,
        "pairing: resolved local-vs-remote"
    );

    let typed_code = form.pairing_code.unwrap_or_default();
    let password = if local {
        fiber_protocol::LOCAL_PAIRING_PASSWORD.to_string()
    } else {
        let code = typed_code.trim();
        if code.len() != 6 || !code.chars().all(|c| c.is_ascii_alphanumeric()) {
            return back_to_system(&hostname, "err", "invalid_code");
        }
        code.to_string()
    };

    match run_pairing(
        &state,
        &hostname,
        local,
        &password,
        Some(&current_user),
        &current_user.username,
        "portal",
    )
    .await
    {
        // A failure still lands on the System tab: that is where the refusal is explained and
        // where the same form waits for the retry.
        Ok(()) => match safe_return_to(form.return_to.as_deref()) {
            Some(path) => Redirect::to(path).into_response(),
            None => back_to_system(&hostname, "ok", "paired"),
        },
        Err(slug) => back_to_system(&hostname, "err", slug),
    }
}

/// Did this Hub's pairing come from the Hub itself, rather than from an operator in the portal
/// (GitLab #61, `plan/control/ble/peripheral.md` §6.5)?
///
/// `device_identities.paired_by_user_id` records who paired, and an invited pairing has nobody to
/// record — that is the whole point of the topic: the operator is at the front panel with a phone,
/// not signed in. Rendered from that column alone the row reads "paired by unknown", which is also
/// exactly what a *lost* record looks like, so the page cannot tell the two apart.
///
/// The `device_pair` audit row is the only other trace, and it carries the origin. Read back here
/// rather than copied into a new column: it needs no migration, and a second copy of a fact is a
/// second thing to keep true. The cost is one indexed read on a page that already makes several,
/// and only for a Hub whose pairing has no user on it.
///
/// A missing or unreadable row answers "no": the honest fallback is the display this already had.
async fn pairing_came_from_an_invite(state: &AppState, hostname: &str) -> bool {
    // `search` is a case-insensitive LIKE across event_type/username/target — it narrows the
    // page, and the exact `target` match below is what decides. Newest first is the default sort.
    let rows = match state
        .auth
        .audit_page(portal_storage::AuditFilter {
            event_type: Some("device_pair".to_string()),
            success: Some(true),
            search: Some(hostname.to_string()),
            limit: 20,
            ..Default::default()
        })
        .await
    {
        Ok((rows, _total)) => rows,
        Err(err) => {
            tracing::warn!(%err, hostname, "failed to read the pairing's audit row");
            return false;
        }
    };

    rows.iter()
        .find(|row| row.target.as_deref() == Some(hostname))
        .and_then(|row| row.details.as_deref())
        .and_then(|details| serde_json::from_str::<serde_json::Value>(details).ok())
        .and_then(|details| details.get("origin")?.as_str().map(str::to_string))
        .is_some_and(|origin| origin != "portal")
}

/// A `pair/invite`'s `origin`, reduced to something safe everywhere it ends up: ASCII letters,
/// digits and `-`/`_`, at most 32 of them, and never empty.
///
/// It travels further than "a log line": [`serve_pair_invites`] splices it into the actor name
/// `invite:{origin}`, which the relay publishes as `admin_username` on `pair/request`, which the
/// Hub mints into the admin certificate it sends back — and that certificate is stored in
/// `device_identities`. It is also the `origin` key of the `device_pair` audit row. So the value
/// is read back by an operator, carried on the wire, and baked into a credential.
///
/// The field exists so an operator can see which surface asked, and the agent only ever sends one
/// of three words — but it arrives over the wire, and "the sender is well-behaved" is not a
/// property to rely on for a value with that reach.
fn sanitize_origin(origin: Option<&str>) -> String {
    let cleaned: String = origin
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    if cleaned.is_empty() {
        "device".to_string()
    } else {
        cleaned
    }
}

/// The pairing code's alphabet and length, mirrored from FB1A
/// (`control-ble/src/peripheral/gatt/pairing_code.rs`, itself a mirror of `control-core`'s
/// `libs/pairing/code.rs`). O, 0, I, 1 and L are absent by design: an operator reads the code off
/// the Hub's display and types it into a phone.
///
/// Mirrored rather than imported because the dependency runs the other way — this binary does not
/// depend on `control-ble`, and the constant is five words of alphabet.
const PAIRING_CODE_CHARSET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
const PAIRING_CODE_LENGTH: usize = 6;

/// A `pair/invite`'s `code`, trimmed and uppercased, or `None` if it is not a pairing code.
///
/// Same rule FB1A applies before it ever publishes the invite — trimmed because a BLE client that
/// appends a newline is not making a mistake worth refusing, uppercased because a phone keyboard
/// hands back lowercase and the alphabet has no lowercase to confuse it with.
///
/// **Why this gate exists here too**, when FB1A already ran it: an invite arrives over the broker,
/// and the check on the far side of a wire is not a check. And the cost of skipping it is not "a
/// pairing fails" — `run_pairing` publishes `pair/request`, and the Hub spends the front-panel
/// window on it whether or not the code is right (`control-core/src/libs/pairing/mod.rs`). A
/// publisher sending `{}` once a second would burn every window the operator opens and make
/// pairing impossible. Refusing the shape costs nothing and touches no device
/// (`plan/control/ble/peripheral.md` §6.5, GitLab #61).
///
/// Stricter than the POST form's own rule, which accepts any six alphanumerics: the form is typed
/// by an admin who is already signed in, this is not.
fn normalize_pairing_code(code: Option<&str>) -> Option<String> {
    let code = code.unwrap_or("").trim().to_ascii_uppercase();
    if code.len() != PAIRING_CODE_LENGTH {
        return None;
    }
    if !code.bytes().all(|b| PAIRING_CODE_CHARSET.contains(&b)) {
        return None;
    }
    Some(code)
}

/// Hostnames whose invited pairing is still running, so a second invite for the same Hub is
/// dropped instead of queued behind it.
type InFlightHosts = std::sync::Arc<parking_lot::Mutex<std::collections::HashSet<String>>>;

/// Holds one hostname in [`InFlightHosts`] for as long as its pairing runs.
///
/// Released on drop rather than at the end of the task body, so a panic inside the exchange
/// cannot leave a Hub permanently un-pairable until the portal restarts.
struct InFlightGuard {
    hosts: InFlightHosts,
    hostname: String,
}

impl InFlightGuard {
    /// `None` when a pairing for this hostname is already running.
    fn claim(hosts: &InFlightHosts, hostname: &str) -> Option<Self> {
        hosts.lock().insert(hostname.to_string()).then(|| Self {
            hosts: hosts.clone(),
            hostname: hostname.to_string(),
        })
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.hosts.lock().remove(&self.hostname);
    }
}

/// The `device_pair` row for an invite the portal refused before touching the device.
///
/// No user, because there is none — nobody signed in for this — and `success = false`, so the
/// operator standing at the Hub wondering why nothing happened has a record naming which surface
/// asked and what was wrong with it.
async fn audit_refused_invite(state: &AppState, hostname: &str, origin: &str, slug: &str) {
    state
        .auth
        .audit(
            "device_pair",
            None,
            None,
            Some(hostname),
            Some(json!({ "origin": origin, "error": slug }).to_string()),
            false,
        )
        .await;
}

/// Consume `pair/invite` events and run the pairing each one asks for (GitLab #61,
/// `plan/control/ble/peripheral.md` §6.5).
///
/// Why this exists at all: the operator is standing at the Hub with a phone, and the phone reaches
/// the Hub over BLE but not the portal over HTTP. FB1A hands the code to the agent, and the agent
/// asks the portal — on the same machine — to do what an admin clicking "Pair" would.
///
/// Three things it deliberately does **not** take from the message:
///
/// - **Whether the device is local.** That is recomputed here with `is_local_device`, exactly as
///   the route does. A payload that could claim locality could claim the fixed local password.
/// - **Which Hub to pair with.** The hostname comes from the topic, so a device can only ever
///   invite a pairing with itself.
/// - **An identity.** There is no portal user behind this, so the pairing is attributed to no one
///   and the audit row carries the origin instead. That is the honest record: someone with
///   physical access to the front panel did this, not an account.
///
/// And three the POST route gets for free from being a route, which this has to state:
///
/// - **The Hub must be one this portal knows** ([`known_hub`], the same lookup that makes an
///   unknown `{hub}` a 404). Without it every topic segment on the broker reaches `run_pairing`,
///   and each unknown one costs the full 30 s `PAIR_TIMEOUT`.
/// - **The code must be a code** ([`normalize_pairing_code`]), checked before the device is
///   touched, because touching it spends the operator's pairing window.
/// - **One pairing per Hub at a time** ([`InFlightGuard`]). Each accepted invite runs as its own
///   task — one slow Hub must not hold up another's — and a repeat for a Hub already pairing is
///   dropped rather than queued.
pub(crate) async fn serve_pair_invites(
    state: AppState,
    mut invites: tokio::sync::mpsc::UnboundedReceiver<crate::state::IngestEvent>,
) {
    let in_flight: InFlightHosts = Default::default();

    while let Some(event) = invites.recv().await {
        let crate::state::IngestEvent::PairInvite { hostname, payload } = event else {
            continue;
        };
        // The origin is wire input, and it reaches an audit row, a log line, the `admin_username`
        // on `pair/request` and — through that — the admin certificate this portal stores. It is
        // bounded and stripped to a plain identifier rather than trusted to be one of the three
        // words the agent sends (see `sanitize_origin`).
        let origin = sanitize_origin(payload.origin.as_deref());

        // The hostname is a raw topic segment: anything that can publish under the prefix can
        // name anything here. Resolving it through the store first is what makes the set of
        // reachable targets the set of Hubs this portal has heard from — and it normalises the
        // spelling, the same way the POST route's `{hub}` is normalised.
        let Some(hostname) = known_hub(&state, &hostname) else {
            tracing::warn!(
                hostname,
                origin,
                "pair/invite names a hostname this portal does not know — ignoring"
            );
            audit_refused_invite(&state, &hostname, &origin, "unknown_hub").await;
            continue;
        };

        let local = is_local_device(&hostname, &state.local_hostname);
        let password = if local {
            fiber_protocol::LOCAL_PAIRING_PASSWORD.to_string()
        } else {
            let Some(code) = normalize_pairing_code(payload.code.as_deref()) else {
                tracing::warn!(
                    hostname,
                    origin,
                    "pair/invite carries no usable pairing code — refusing before the device is \
                     asked, because asking spends the window on its display"
                );
                audit_refused_invite(&state, &hostname, &origin, "invalid_code").await;
                continue;
            };
            code
        };

        // No audit row for this one: the invite it duplicates is being acted on right now, and a
        // row per retry would say only that the phone was tapped twice.
        let Some(guard) = InFlightGuard::claim(&in_flight, &hostname) else {
            tracing::warn!(
                hostname,
                origin,
                "a pairing for this Hub is already running — dropping the invite"
            );
            continue;
        };

        tracing::info!(hostname, origin, local, "pair/invite accepted");
        let task_state = state.clone();
        tokio::spawn(async move {
            let _guard = guard;
            match run_pairing(
                &task_state,
                &hostname,
                local,
                &password,
                None,
                &format!("invite:{origin}"),
                &origin,
            )
            .await
            {
                Ok(()) => tracing::info!(hostname, origin, "paired on the device's invitation"),
                Err(slug) => {
                    tracing::warn!(hostname, origin, slug, "invited pairing failed");
                    audit_refused_invite(&task_state, &hostname, &origin, slug).await;
                }
            }
        });
    }
}

/// The pairing exchange itself, shared by the operator's POST and by a device that asks for it
/// over `pair/invite` (GitLab #61).
///
/// Extracted rather than duplicated because the interesting half is the refusals — a mismatched CA
/// key, a response that cannot be decrypted, a window that was never opened — and a second copy of
/// that reasoning is a second set of them to get wrong.
///
/// `actor` is the portal user to attribute the pairing to, and is `None` for a pairing nobody
/// signed in for; `actor_name` is what goes on the wire as the requester, and `origin` is what the
/// audit row records as the surface that asked. The error is the flash slug the route renders.
pub(crate) async fn run_pairing(
    state: &AppState,
    hostname: &str,
    local: bool,
    password: &str,
    actor: Option<&CurrentUser>,
    actor_name: &str,
    origin: &str,
) -> Result<(), &'static str> {
    let payload = match state.relay.pair(hostname, actor_name, local).await {
        Ok(payload) => payload,
        Err(RelayError::Timeout) => return Err("timeout"),
        Err(err) => {
            tracing::error!(%err, hostname, "pairing request failed");
            return Err("pair_failed");
        }
    };

    // A refusal is the device saying no — overwhelmingly "Not in pairing
    // mode", i.e. the window on the LCD was never opened or has already been
    // spent. It has nothing to do with the code, which the device never even
    // compares, so telling the operator to check theirs sends them looking in
    // the wrong place.
    if !payload.success {
        tracing::warn!(hostname, error = ?payload.error, "device refused pairing");
        return Err("not_pairing_mode");
    }

    let (Some(enc), Some(ca_public_key)) = (payload.encrypted_private_key, payload.ca_public_key)
    else {
        tracing::warn!(
            hostname,
            "pair/response missing key material despite success=true"
        );
        return Err("not_pairing_mode");
    };

    let seed = match fiber_protocol::decrypt_pair_response(
        password,
        &enc.salt,
        &enc.nonce,
        &enc.ciphertext,
    ) {
        Ok(seed) => seed,
        Err(err) => {
            tracing::warn!(%err, hostname, local, "pairing response decryption failed");
            // Nobody typed anything on the local path, so "check the code" is
            // meaningless there: it means the two sides disagree about the
            // fixed password — an old agent build, or a drifted mirror of the
            // constant.
            return Err(if local { "local_decrypt" } else { "bad_code" });
        }
    };

    // Stricter than v1 (which only logged a mismatch): refuse to store a CA
    // seed whose derived public key doesn't match what the device itself
    // reported.
    if !pubkey_matches(&seed, &ca_public_key) {
        tracing::error!(
            hostname,
            "pairing aborted: derived public key does not match device's ca_public_key — refusing to store"
        );
        return Err("mismatch");
    }

    if let Err(err) = state
        .keys
        .store_pairing(
            hostname,
            payload.ca_id,
            ca_public_key,
            seed,
            payload.admin_certificate,
            actor.map(|u| u.id),
        )
        .await
    {
        tracing::error!(%err, hostname, "failed to store pairing");
        return Err("store_failed");
    }

    state
        .auth
        .audit(
            "device_pair",
            actor,
            None,
            Some(hostname),
            Some(serde_json::json!({ "origin": origin }).to_string()),
            true,
        )
        .await;
    Ok(())
}

async fn unpair_device(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
) -> Response {
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };

    match state.keys.unpair(&hostname).await {
        Ok(()) => {
            state
                .auth
                .audit(
                    "device_unpair",
                    Some(&current_user),
                    None,
                    Some(&hostname),
                    None,
                    true,
                )
                .await;
            // Amendment 2026-10-14: an unpaired Hub's probe reminders close.
            state.calibration.forget_hub(&hostname).await;
            back_to_system(&hostname, "ok", "unpaired")
        }
        Err(err) => {
            tracing::warn!(%err, hostname, "unpair failed");
            back_to_system(&hostname, "err", "unpair_failed")
        }
    }
}

// ---------------------------------------------------------------------
// POST /hubs/{hub}/system/{restart|power-off|factory-reset}
// ---------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct ReasonForm {
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
struct FactoryResetForm {
    reason: String,
    confirm_hostname: String,
    post_action: String,
}

/// factory-reset validation (§8.3): reason must have at least 8
/// non-whitespace characters and at most 256 characters total,
/// `confirm_hostname` must equal the addressed Hub's hostname exactly, and
/// `post_action` must be `"reboot"` or `"power_off"`. Pure and called
/// before ever touching `relay` — a failure here means `relay.execute` is
/// never invoked (the capture-friendly seam for the validation-matrix
/// tests below).
fn validate_factory_reset(form: &FactoryResetForm, hostname: Option<&str>) -> Result<(), String> {
    let non_ws_count = form.reason.chars().filter(|c| !c.is_whitespace()).count();
    if non_ws_count < 8 {
        return Err(tr("monitoring.device.factory_reset_reason_too_short"));
    }
    if form.reason.chars().count() > 256 {
        return Err(tr("monitoring.device.factory_reset_reason_too_long"));
    }

    match hostname {
        None => return Err(tr("monitoring.shared.no_device_yet")),
        Some(h) if form.confirm_hostname != h => {
            return Err(tr("monitoring.device.factory_reset_hostname_mismatch"))
        }
        Some(_) => {}
    }

    if form.post_action != "reboot" && form.post_action != "power_off" {
        return Err(tr("monitoring.device.factory_reset_invalid_post_action"));
    }
    Ok(())
}

/// The audit event each system command writes (§7: restart, power-off and
/// factory reset are audited "nově"). One name per command, in the family
/// `pair`/`unpair` already use, so the audit page reads as one vocabulary.
fn audit_event(command_type: &str) -> &'static str {
    match command_type {
        "restart_application" => "device_restart",
        "power_off" => "device_power_off",
        "factory_reset" => "device_factory_reset",
        _ => "device_system_command",
    }
}

/// Shared execute path for the three system commands: mint/reuse the
/// user's signing key, run the certificate preflight — universal now that
/// every verb has a permission, rather than v1's hand-picked quartet
/// (plan/portal/mqtt/design.md §4.4 step 3) — then hand off to the relay. `permissions_for(current_user)` is exactly
/// what's requested from `ensure_user_key`, so the preflight below can only
/// ever fail if a route's role gate and `perms::permissions_for` have
/// drifted apart — the check still runs, since that's the documented
/// contract, not because it's expected to trip here.
///
/// `hostname` comes from the path (§3): the relay publishes to the Hub the
/// URL names, never to whichever device the store lists first.
async fn run_system_command(
    state: &AppState,
    current_user: &CurrentUser,
    hostname: &str,
    command_type: &str,
    params: serde_json::Value,
    reason: Option<String>,
) -> Response {
    Cmd::new(state, current_user, command_type)
        .relay(
            hostname,
            params,
            reason,
            audit_event(command_type),
            hostname,
            json!({ "command": command_type }),
        )
        .await
}

/// Admin (§3.3) — tightened from the shipped manager+, so the gate agrees
/// with the tab the card lives on.
async fn restart(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<ReasonForm>,
) -> Response {
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    run_system_command(
        &state,
        &current_user,
        &hostname,
        "restart_application",
        json!({}),
        non_empty(form.reason),
    )
    .await
}

async fn power_off(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<ReasonForm>,
) -> Response {
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    let Some(reason) = non_empty(form.reason) else {
        return Cmd::new(&state, &current_user, "power_off")
            .fail(&tr("monitoring.device.power_off_reason_required"));
    };
    run_system_command(
        &state,
        &current_user,
        &hostname,
        "power_off",
        json!({}),
        Some(reason),
    )
    .await
}

async fn factory_reset(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<FactoryResetForm>,
) -> Response {
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    if let Err(msg) = validate_factory_reset(&form, Some(hostname.as_str())) {
        return Cmd::new(&state, &current_user, "factory_reset").fail(&msg);
    }

    let params = json!({ "post_action": form.post_action });
    run_system_command(
        &state,
        &current_user,
        &hostname,
        "factory_reset",
        params,
        Some(form.reason.trim().to_string()),
    )
    .await
}

// ---------------------------------------------------------------------
// POST /hubs/{hub}/system/radio-mode
// ---------------------------------------------------------------------

/// "Změnit rádio Hubu" (devices.hub.md §3.4, UI-24 as amended 2026-09-27;
/// plan/control/radio/node_registry.md §11, §12). It replaced the transport
/// switch that was a factory reset: the Hub changes `radio.mode` in place
/// and restarts its radio services and this Portal, and deletes nothing.
#[derive(Deserialize)]
struct RadioModeForm {
    mode: String,
    confirm_hostname: String,
    #[serde(default)]
    reason: String,
    /// What happens to the Nodes on a carrier the new mode switches off (§12): `move`,
    /// `remove` or `keep`. Asked, and required, only when there are such Nodes.
    #[serde(default)]
    nodes: String,
}

/// The signed verb (`fiber_protocol` registry, admin level).
const RADIO_SET_MODE: &str = "radio-set-mode";

/// The one choice "Změnit rádio Hubu" asks for all the Nodes a switch leaves on a carrier it
/// switches off (node_registry.md §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodesChoice {
    /// „převést na nové rádio“: a signed `node-move` per Node, to the carrier the new mode runs.
    Move,
    /// „smazat“: a signed `node-remove` per Node.
    Remove,
    /// „nechat na starém protokolu“: nothing; they show „rádio na tomto Hubu vypnuté“.
    Keep,
}

impl NodesChoice {
    fn parse(text: &str) -> Option<NodesChoice> {
        match text.trim() {
            "move" => Some(NodesChoice::Move),
            "remove" => Some(NodesChoice::Remove),
            "keep" => Some(NodesChoice::Keep),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            NodesChoice::Move => "move",
            NodesChoice::Remove => "remove",
            NodesChoice::Keep => "keep",
        }
    }
}

/// Validation before anything is signed: one of the four modes, the typed
/// hostname, a reason of at most 256 characters, and not the mode the Hub
/// already runs (`current`, known for the Hub this Portal runs on). Whether
/// `dual` is refused: it is service-only, set over SSH, never from the UI. Whether
/// the hardware can run the mode is the Hub's to decide — this process
/// cannot see the concentrator — and its refusal comes back as the
/// command's error.
fn validate_radio_mode(
    form: &RadioModeForm,
    hostname: &str,
    current: Option<control_radio::Mode>,
) -> Result<control_radio::Mode, String> {
    let mode = control_radio::setmode::parse_mode(&form.mode)
        .ok()
        .filter(|mode| *mode != control_radio::Mode::Dual)
        .ok_or_else(|| tr("monitoring.hubs.radio_change_invalid_mode"))?;
    if form.confirm_hostname.trim() != hostname {
        return Err(tr("monitoring.device.factory_reset_hostname_mismatch"));
    }
    if form.reason.chars().count() > 256 {
        return Err(tr("monitoring.hubs.radio_change_reason_too_long"));
    }
    if current == Some(mode) {
        return Err(tr("monitoring.hubs.radio_change_unchanged"));
    }
    Ok(mode)
}

/// The Node choice a switch needs, checked before anything is signed: `None` when the switch
/// asks nothing (no Node on a carrier it switches off), the choice when it does. A missing or
/// unknown choice is refused, and so is a move the new mode has no carrier for (`off`).
fn validate_nodes_choice(
    raw: &str,
    switch: Option<&crate::radio::NodeSwitch>,
) -> Result<Option<NodesChoice>, String> {
    let Some(switch) = switch else {
        return Ok(None);
    };
    let choice =
        NodesChoice::parse(raw).ok_or_else(|| tr("monitoring.hubs.radio_change_nodes_required"))?;
    if choice == NodesChoice::Move && switch.move_to.is_none() {
        return Err(tr("monitoring.hubs.radio_change_nodes_move_unavailable"));
    }
    Ok(Some(choice))
}

/// Admin (§3.4, §7). Only the Hub this Portal runs on — the Server: a
/// client Hub takes the cluster's radio from the Server (UI-24), and the
/// current mode, which the typed confirm is checked against, is known only
/// for the local Hub.
///
/// **The Nodes first** (node_registry.md §12). When the new mode switches off a carrier this
/// Hub's registry holds Nodes on, the form carries one choice for all of them, and the Node step
/// runs **before** `radio-set-mode`, while that carrier is still on: [`node_step`], one signed
/// command and one audit row per Node. A Node that fails does not stop the others, nor the
/// switch; the result names the ones that failed. A switch that switches nothing off with Nodes
/// on it asks nothing.
///
/// Audited `hub_radio_mode_set` with the mode it came from, the one it asked for and the Node
/// choice; the Hub adds its own `SET_RADIO_MODE` row when it writes.
async fn set_radio_mode(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<RadioModeForm>,
) -> Response {
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    let cmd = Cmd::new(&state, &current_user, RADIO_SET_MODE);
    if !is_local_device(&hostname, &state.local_hostname) {
        return cmd.fail(&tr("monitoring.hubs.radio_change_server_only"));
    }
    let current = state.radio.mode();
    let mode = match validate_radio_mode(&form, &hostname, current) {
        Ok(mode) => mode,
        Err(msg) => return cmd.fail(&msg),
    };
    let switch = state.radio.node_switch(mode);
    let choice = match validate_nodes_choice(&form.nodes, switch.as_ref()) {
        Ok(choice) => choice,
        Err(msg) => return cmd.fail(&msg),
    };

    // 1. The Nodes, while their carrier is still on.
    let mut summary: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    if let (Some(switch), Some(choice)) = (switch.as_ref(), choice) {
        let step = match (choice, switch.move_to) {
            (NodesChoice::Move, Some(to)) => Some(NodeStep::Move { to }),
            (NodesChoice::Remove, _) => Some(NodeStep::Remove),
            _ => None,
        };
        match step {
            Some(step) => {
                let report = node_step(&state, &current_user, &hostname, &switch.nodes, step).await;
                summary.push(report.message(step));
                failed = report.failed.clone();
                failed.extend(report.not_sent.iter().cloned());
            }
            None => summary.push(
                tr("monitoring.hubs.radio_change_nodes_kept")
                    .replace("{count}", &switch.nodes.len().to_string()),
            ),
        }
    }

    // 2. The switch, whatever the Node step did.
    let details = json!({
        "command": RADIO_SET_MODE,
        "from": current.map(|m| m.as_str()),
        "to": mode.as_str(),
        "nodes": choice.map(NodesChoice::as_str),
        "nodes_failed": failed,
    });
    let outcome = match cmd
        .execute(
            &hostname,
            json!({ "mode": mode.as_str() }),
            non_empty(form.reason.trim().to_string()),
        )
        .await
    {
        Ok(outcome) => {
            cmd.audit("hub_radio_mode_set", &hostname, details, &outcome)
                .await;
            outcome
        }
        Err(refused) => refused,
    };
    if summary.is_empty() {
        return cmd.done(outcome);
    }
    if !super::command::succeeded(&outcome) {
        summary.push(tr("monitoring.hubs.radio_change_nodes_switch_failed"));
    }
    let mut message = outcome.message.clone().unwrap_or_default();
    for line in summary {
        if !message.is_empty() {
            message.push(' ');
        }
        message.push_str(&line);
    }
    cmd.done(crate::relay::CommandOutcome {
        message: Some(message),
        ..outcome
    })
}

// ---------------------------------------------------------------------
// POST /hubs/{hub}/radio/northbridge-update
// ---------------------------------------------------------------------

/// The signed verb (`fiber_protocol` registry: `radio.manage`, admin).
const NORTHBRIDGE_UPDATE: &str = "northbridge-update";

#[derive(Deserialize)]
struct FirmwareUpdateForm {
    /// „Vynutit“: the checkbox, present only when ticked. Shared by the NB and SB updates.
    #[serde(default)]
    force: Option<String>,
}

/// „Aktualizovat firmware NB“ (devices.hub.md, dodatek 2026-10-09;
/// plan/control/radio/northbridge_update.md §3.4, amended 2026-10-09): the signed
/// `northbridge-update {force}`, which the Hub's agent turns into the CLI's own
/// `northbridge-update --radio p2p --no-wait [--force]`. The bundled image only; nothing is
/// uploaded from here.
///
/// **Admin**, the tier and permission of `radio-set-mode`: the P2P radio stops for about a
/// minute. Refused before anything is signed when the card would not offer it: no Northbridge
/// row (no P2P on this Hub), no `update` object (the image bundles no firmware), or an update
/// already `flashing`. Audited `hub_northbridge_update` with `force` and the two versions.
///
/// On success the answer carries the radio card out of band, following the update: it says
/// „Probíhá aktualizace…“ and polls itself until the retained document reports the outcome.
async fn northbridge_update(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<FirmwareUpdateForm>,
) -> Response {
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    let cmd = Cmd::new(&state, &current_user, NORTHBRIDGE_UPDATE);
    let update = {
        let portal_state = state.store.snapshot();
        portal_state
            .device(&hostname)
            .and_then(|d| render::radio_card_ctx(d, None, &[]).nb_firmware)
            .and_then(|nb| nb.update)
    };
    let Some(update) = update else {
        return cmd.fail(&tr("monitoring.hubs.nb_update_unavailable"));
    };
    if update.state == "flashing" {
        return cmd.fail(&tr("monitoring.hubs.nb_update_already_running"));
    }
    let force = matches!(
        form.force.as_deref().map(str::trim),
        Some("on" | "true" | "1")
    );
    let since = unix_now();
    let outcome = match cmd
        .execute(&hostname, json!({ "force": force }), None)
        .await
    {
        Ok(outcome) => outcome,
        Err(refused) => return cmd.done(refused),
    };
    cmd.audit(
        "hub_northbridge_update",
        &hostname,
        json!({
            "command": NORTHBRIDGE_UPDATE,
            "force": force,
            "running": update.running,
            "bundled": update.bundled,
        }),
        &outcome,
    )
    .await;
    if !super::command::succeeded(&outcome) {
        return cmd.done(outcome);
    }
    let portal_state = state.store.snapshot();
    let radio = portal_state.device(&hostname).map(|d| {
        radio_card_ctx(
            &state,
            d,
            portal_state.radio_gateways.as_ref(),
            &current_user,
            Some((render::NbUpdatePhase::Wait, since)),
        )
    });
    render_fragment(
        &state,
        "partials/hub_nb_update_sent.html",
        render::page_ctx(
            context! {
                cmd => cmd.ctx(outcome),
                radio => radio,
            },
            Some(&current_user),
        ),
    )
}

// ---------------------------------------------------------------------
// POST /hubs/{hub}/system/southbridge-update
// ---------------------------------------------------------------------

/// The signed verb (`fiber_protocol` registry: `radio.manage`, admin).
const SOUTHBRIDGE_UPDATE: &str = "southbridge-update";

/// „Aktualizovat firmware SB“ (devices.hub.md, dodatek 2026-10-09;
/// plan/system/southbridge_update.md §3.7): the signed `southbridge-update {force}`, which the
/// Hub's `fiber_app` answers at once and carries out on a thread of its own. The bundled image
/// only; nothing is uploaded from here.
///
/// **Admin**, like `northbridge-update`: sensor power and the line LEDs stop for about 15 s.
/// Refused before anything is signed when the card would not offer it: no `southbridge` block
/// or `update` object, no bundled image, or an update already `running`. Audited
/// `hub_southbridge_update` with `force` and the two versions.
///
/// On success the answer carries the "Firmware SB" card out of band, following the update:
/// „Probíhá aktualizace…“, polling itself until `update.state` is `idle` or `failed` again.
async fn southbridge_update(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<FirmwareUpdateForm>,
) -> Response {
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    let cmd = Cmd::new(&state, &current_user, SOUTHBRIDGE_UPDATE);
    let sb = {
        let portal_state = state.store.snapshot();
        portal_state
            .device(&hostname)
            .and_then(|d| d.system.southbridge.as_ref())
            .map(render::SouthbridgeCtx::from_info)
    };
    let Some(sb) = sb.filter(|sb| sb.has_update && sb.bundled.is_some()) else {
        return cmd.fail(&tr("monitoring.hubs.sb_update_unavailable"));
    };
    if sb.running() {
        return cmd.fail(&tr("monitoring.hubs.sb_update_already_running"));
    }
    let force = matches!(
        form.force.as_deref().map(str::trim),
        Some("on" | "true" | "1")
    );
    let since = unix_now();
    let outcome = match cmd
        .execute(&hostname, json!({ "force": force }), None)
        .await
    {
        Ok(outcome) => outcome,
        Err(refused) => return cmd.done(refused),
    };
    cmd.audit(
        "hub_southbridge_update",
        &hostname,
        json!({
            "command": SOUTHBRIDGE_UPDATE,
            "force": force,
            "running": sb.version,
            "bundled": sb.bundled,
        }),
        &outcome,
    )
    .await;
    if !super::command::succeeded(&outcome) {
        return cmd.done(outcome);
    }
    let portal_state = state.store.snapshot();
    let card = portal_state.device(&hostname).map(|d| {
        render::sb_firmware_ctx(
            d,
            current_user.is_admin(),
            Some((render::NbUpdatePhase::Wait, since)),
            unix_now(),
        )
    });
    render_fragment(
        &state,
        "partials/hub_sb_update_sent.html",
        render::page_ctx(
            context! {
                cmd => cmd.ctx(outcome),
                sb_firmware => card,
            },
            Some(&current_user),
        ),
    )
}

// ---------------------------------------------------------------------
// The Node step: one signed command per Node
// ---------------------------------------------------------------------

/// What one bulk Node step does to each Node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeStep {
    /// A signed `node-move` to `to`.
    Move { to: portal_domain::Transport },
    /// A signed `node-remove` on the Node's own carrier.
    Remove,
}

/// What a [`node_step`] did.
#[derive(Debug, Default)]
struct NodeStepReport {
    total: usize,
    done: usize,
    /// DevEUIs the Hub answered with anything but success.
    failed: Vec<String>,
    /// DevEUIs never sent, after a refusal on this side of the wire.
    not_sent: Vec<String>,
    /// That refusal: a certificate or key problem, which would refuse every Node alike.
    refused: Option<String>,
}

impl NodeStepReport {
    /// One sentence for the result: how many of how many, and which did not.
    fn message(&self, step: NodeStep) -> String {
        let carrier =
            |c: portal_domain::Transport| tr(&format!("monitoring.transport.{}", c.as_str()));
        let mut message = match step {
            NodeStep::Move { to } => tr("monitoring.hubs.move_nodes_result")
                .replace("{to}", &carrier(to))
                .replace("{moved}", &self.done.to_string())
                .replace("{total}", &self.total.to_string()),
            NodeStep::Remove => tr("monitoring.hubs.radio_change_nodes_removed")
                .replace("{done}", &self.done.to_string())
                .replace("{total}", &self.total.to_string()),
        };
        if !self.failed.is_empty() {
            let key = match step {
                NodeStep::Move { .. } => "monitoring.hubs.move_nodes_not_moved",
                NodeStep::Remove => "monitoring.hubs.radio_change_nodes_not_removed",
            };
            message.push(' ');
            message.push_str(&tr(key).replace("{nodes}", &self.failed.join(", ")));
        }
        if let Some(reason) = &self.refused {
            message.push(' ');
            message.push_str(&tr("monitoring.hubs.move_nodes_refused").replace("{reason}", reason));
        }
        message
    }
}

/// The bulk Node step of "Přesunout všechny Nody" (R6) and of "Změnit rádio Hubu" (§12): **one
/// signed command and one audit row per Node**, as the same action from each Node's page would
/// be. There is no bulk verb.
///
/// - **Move:** `node-move {dev_eui, to}`, audited `node_move` (`from`, `to`, `bulk: true`).
/// - **Remove:** `node-remove {radio, dev_eui}` on the Node's carrier, audited `node_remove`
///   (`transport`, `bulk: true`); a removed Node's Portal-side records go as on its own page
///   ([`super::node_routes::forget_removed_node`]).
///
/// A Node the Hub refuses does not stop the others. One refused on this side, before anything
/// was signed (a certificate or key problem), would be refused for every Node, so the rest is not
/// sent and the report says why.
async fn node_step(
    state: &AppState,
    user: &CurrentUser,
    hostname: &str,
    nodes: &[(String, portal_domain::Transport)],
    step: NodeStep,
) -> NodeStepReport {
    let cmd = Cmd::new(
        state,
        user,
        match step {
            NodeStep::Move { .. } => NODE_MOVE,
            NodeStep::Remove => NODE_REMOVE,
        },
    );
    let mut report = NodeStepReport {
        total: nodes.len(),
        ..NodeStepReport::default()
    };
    for (i, (dev_eui, carrier)) in nodes.iter().enumerate() {
        let params = match step {
            NodeStep::Move { to } => json!({ "dev_eui": dev_eui, "to": to.as_str() }),
            NodeStep::Remove => json!({ "radio": carrier.as_str(), "dev_eui": dev_eui }),
        };
        let outcome = match cmd.execute(hostname, params, None).await {
            Ok(outcome) => outcome,
            Err(refusal) => {
                report.refused = refusal.message;
                report
                    .not_sent
                    .extend(nodes[i..].iter().map(|(d, _)| d.clone()));
                break;
            }
        };
        let ok = super::command::succeeded(&outcome);
        match step {
            NodeStep::Move { to } => {
                cmd.audit(
                    "node_move",
                    dev_eui,
                    json!({
                        "from": carrier.as_str(),
                        "to": to.as_str(),
                        "bulk": true,
                        "message": outcome.message,
                    }),
                    &outcome,
                )
                .await;
            }
            NodeStep::Remove => {
                if ok {
                    super::node_routes::forget_removed_node(state, hostname, dev_eui).await;
                }
                cmd.audit(
                    "node_remove",
                    dev_eui,
                    json!({
                        "transport": carrier.as_str(),
                        "bulk": true,
                        "message": outcome.message,
                    }),
                    &outcome,
                )
                .await;
            }
        }
        if ok {
            report.done += 1;
        } else {
            report.failed.push(dev_eui.clone());
        }
    }
    report
}

// ---------------------------------------------------------------------
// POST /hubs/{hub}/radio/move-nodes
// ---------------------------------------------------------------------

/// "Přesunout všechny Nody na <rádio>": the typed confirm and the carrier the card offered.
#[derive(Deserialize)]
struct MoveNodesForm {
    #[serde(default)]
    to: String,
    #[serde(default)]
    confirm_hostname: String,
}

/// The signed verb each Node's move is (control.radio.md §7, amendment 2026-09-27).
const NODE_MOVE: &str = "node-move";
/// The signed verb each Node's removal is, the one a Node's own page sends.
const NODE_REMOVE: &str = "node-remove";

/// "Přesunout všechny Nody na <rádio>" (node_registry.md §11; devices.hub.md, dodatek
/// 2026-09-27, R6): after the Hub's radio changed, every Node the registry keeps on the carrier
/// it switched off moves to the one it runs, through [`node_step`].
///
/// **Admin, the local Hub only, with the typed hostname**, as "Změnit rádio Hubu": the Nodes
/// come from this Hub's registry. `to` must be the carrier the card offered; a page drawn
/// before another mode change is refused rather than moving Nodes somewhere unintended.
///
/// **The Nodes themselves are not told** (§10 Q1). The result says so.
async fn move_all_nodes(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<MoveNodesForm>,
) -> Response {
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    let cmd = Cmd::new(&state, &current_user, NODE_MOVE);
    if !is_local_device(&hostname, &state.local_hostname) {
        return cmd.fail(&tr("monitoring.hubs.radio_change_server_only"));
    }
    if form.confirm_hostname.trim() != hostname {
        return cmd.fail(&tr("monitoring.device.factory_reset_hostname_mismatch"));
    }
    let Some(plan) = state.radio.bulk_move() else {
        return cmd.fail(&tr("monitoring.hubs.move_nodes_none"));
    };
    if form.to.trim() != plan.to.as_str() {
        return cmd.fail(&tr("monitoring.hubs.move_nodes_stale"));
    }
    let nodes: Vec<(String, portal_domain::Transport)> = plan
        .dev_euis
        .iter()
        .map(|d| (d.clone(), plan.from))
        .collect();
    let step = NodeStep::Move { to: plan.to };
    let report = node_step(&state, &current_user, &hostname, &nodes, step).await;
    let message = report.message(step);
    if report.done == report.total {
        cmd.ok(&message)
    } else {
        cmd.fail(&message)
    }
}

// ---------------------------------------------------------------------
// POST /hubs/{hub}/sensors/{line}/threshold
// ---------------------------------------------------------------------

/// `partials/limits_editor.html`'s POST body: the four visible bounds.
/// Kept as raw strings (not `Option<f64>`) so a blank field, which `Form`'s
/// float deserialization would reject outright, parses here instead, where
/// blank can be told apart from invalid and refused in words.
///
/// No `alarm_low`/`alarm_high`: the UI no longer offers that pair
/// (2026-09-26), and [`wired_threshold_params`] sets it to the critical bounds.
#[derive(Deserialize)]
struct ThresholdForm {
    #[serde(default)]
    critical_low: String,
    #[serde(default)]
    warning_low: String,
    #[serde(default)]
    warning_high: String,
    #[serde(default)]
    critical_high: String,
    /// The Hystereze field (alarm_hysteresis.md §5); see `node_routes::parse_hysteresis`.
    #[serde(default)]
    hysteresis: Option<String>,
    /// The "Mez zapnuta" checkbox (#167); unchecked submits no key, so `None` = off.
    #[serde(default)]
    enabled: Option<String>,
}

/// `""` (after trimming) -> `None`; otherwise parses as `f64` or errors —
/// the pure, directly-testable seam for the "blank means unset, not zero"
/// contract.
fn parse_optional_float(raw: &str, field: &str) -> Result<Option<f64>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<f64>()
        .map(Some)
        .map_err(|_| format!("{field} must be a number or blank"))
}

/// The params of a wired `set_threshold`, in the shape control-core reads:
/// `{line, thresholds: {…six bounds…}}` (`authorization/manager.rs`
/// `build_command_from_challenge`, and `mqtt/monitor.rs` `build_dev_command`).
/// Used by this route and by the bulk panel, so the two cannot drift.
///
/// All four visible bounds are required: control-core turns a missing one
/// into 0.0, which on the device is a 0 °C limit, and v1 required them too.
/// `bounds` is `[critical_low, warning_low, warning_high, critical_high]`.
///
/// The alarm pair is set to the critical bounds (devices.hub.md §3.2,
/// amended 2026-09-28). The UI does not offer it, since it is a v1 leftover
/// the device reads as a second warning bound, and the device's default
/// 0 / 100 °C does not disable it for a probe below 0 °C: control-core
/// grades anything under `alarm_low` as Warning. A pair equal to the
/// critical bounds lies inside the critical zone, where Critical wins, so
/// it has no effect whatever the bounds are.
///
/// `hysteresis` is the one the line is graded with (decision #41); the Hub uses it for its
/// own LCD, LEDs and buzzer only (alarm_hysteresis.md §7), and an older Hub ignores it.
///
/// `enabled` is the band's switch (#167): off keeps the bounds and grades nothing. An
/// older Hub ignores the key and keeps grading.
pub(crate) fn wired_threshold_params(
    line: u8,
    bounds: [Option<f64>; 4],
    hysteresis: f64,
    enabled: bool,
) -> Result<serde_json::Value, String> {
    let [Some(critical_low), Some(warning_low), Some(warning_high), Some(critical_high)] = bounds
    else {
        return Err(tr("monitoring.threshold_form.err_all_four"));
    };
    Ok(json!({
        "line": line,
        "thresholds": {
            "critical_low": critical_low,
            "warning_low": warning_low,
            "warning_high": warning_high,
            "critical_high": critical_high,
            "alarm_low": critical_low,
            "alarm_high": critical_high,
            "hysteresis": hysteresis,
            "enabled": enabled,
        },
    }))
}

/// Stores a line's bounds and hysteresis in the portal's own row (scope `hub`, decision #41:
/// the portal is the source of truth and grades the line against it) before the command
/// leaves, and answers the hysteresis the command carries.
pub(super) async fn store_hub_line(
    state: &AppState,
    hostname: &str,
    line: u8,
    bounds: [Option<f64>; 4],
    hysteresis: Option<Option<f64>>,
    enabled: bool,
    actor: &str,
) -> f64 {
    let scope = portal_storage::THRESHOLD_SCOPE_HUB;
    let entity = line.to_string();
    let [critical_low, warning_low, warning_high, critical_high] = bounds;
    let row = portal_storage::NewFieldThreshold {
        hostname: hostname.to_string(),
        scope: scope.to_string(),
        entity: entity.clone(),
        field: WIRED_FIELD.to_string(),
        critical_low,
        warning_low,
        warning_high,
        critical_high,
        enabled,
    };
    let actor = actor.to_string();
    let stored = match state
        .db
        .write(move |s| s.upsert_field_threshold(&row, Some(&actor)))
        .await
    {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(%err, hostname, line, "a Hub line's bounds were not stored");
            false
        }
    };
    let _ = super::node_routes::save_hysteresis(
        state,
        hostname,
        scope,
        &entity,
        WIRED_FIELD,
        hysteresis,
    )
    .await;
    let h = super::node_routes::effective_hysteresis(state, hostname, scope, &entity, WIRED_FIELD)
        .await;
    // Every page reads the line from the store: it shows the Portal's bounds from now on,
    // whether or not the Hub takes the command (decision #41).
    if stored {
        state
            .store
            .set_line_limits(hostname, line, line_limits(bounds, h, enabled));
    }
    h
}

/// A wired line's bounds as the store holds them; `alarm_*` mirror the critical pair, as
/// the Hub's own echo does.
pub(crate) fn line_limits(
    [critical_low, warning_low, warning_high, critical_high]: [Option<f64>; 4],
    hysteresis: f64,
    enabled: bool,
) -> crate::state::SensorThresholds {
    crate::state::SensorThresholds {
        critical_low,
        warning_low,
        warning_high,
        critical_high,
        alarm_low: critical_low,
        alarm_high: critical_high,
        hysteresis: Some(hysteresis),
        enabled: Some(enabled),
    }
}

/// The one field of a Hub line's `hub` row.
pub(super) const WIRED_FIELD: &str = "temperature";

/// Manager+ (§7). `line` is 0..7 on the wire and in this path; the UI
/// labels the same line 1-based (§3.2 numbering).
async fn set_threshold(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path((hub, line)): Path<(String, u8)>,
    Form(form): Form<ThresholdForm>,
) -> Response {
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    if line > 7 {
        return not_found(&state, Some(&current_user));
    }

    let parsed = (|| -> Result<_, String> {
        Ok([
            parse_optional_float(&form.critical_low, "critical_low")?,
            parse_optional_float(&form.warning_low, "warning_low")?,
            parse_optional_float(&form.warning_high, "warning_high")?,
            parse_optional_float(&form.critical_high, "critical_high")?,
        ])
    })();
    // Checked before anything is stored; the effective hysteresis is known only after.
    let checked = parsed.and_then(|bounds| {
        wired_threshold_params(line, bounds, 0.0, true)?;
        let [cl, wl, wh, ch] = bounds;
        let h = super::node_routes::parse_hysteresis(form.hysteresis.as_deref(), (cl, wl, wh, ch))?;
        Ok((bounds, h))
    });
    let (bounds, hysteresis) = match checked {
        Ok(checked) => checked,
        Err(msg) => {
            return Cmd::new(&state, &current_user, "set_threshold").fail(&msg);
        }
    };
    let enabled = form.enabled.is_some();
    let hysteresis = store_hub_line(
        &state,
        &hostname,
        line,
        bounds,
        hysteresis,
        enabled,
        &current_user.username,
    )
    .await;
    let params = match wired_threshold_params(line, bounds, hysteresis, enabled) {
        Ok(params) => params,
        Err(msg) => {
            return Cmd::new(&state, &current_user, "set_threshold").fail(&msg);
        }
    };

    // The Portal holds the bounds now (decision #41); the Hub's copy is secondary.
    let cmd = Cmd::new(&state, &current_user, "set_threshold");
    cmd.dispatch_stored(
        &hostname,
        params,
        "sensor_threshold_set",
        &format!("{hostname}/line-{line}"),
        json!({ "line": line }),
    )
    .await
}

// ---------------------------------------------------------------------
// GET /hubs/{hub}/sensors/{line}/row
// POST /hubs/{hub}/sensors/{line}/{name|location}
// ---------------------------------------------------------------------

/// One row of the Sensors tab (`partials/hub_sensor_row.html`), refetched by
/// the row itself on the `sensor-{hostname}-{line}` nudge: the device echoes
/// every `set_threshold`/`set_sensor_*` back in `config/state`, and that echo
/// is what repaints the row (devices.hub.md §3.2, §6).
async fn sensor_row(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path((hub, line)): Path<(String, u8)>,
) -> Response {
    let portal_state = state.store.snapshot();
    let Some(hostname) = known_hub_in(&portal_state, &state.cluster, &hub) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // HS-1 (Hub sweep 2026-09-26): the map is keyed lowercase and `known_hub_in` answers the
    // display spelling, so the row fetch the SSE nudge triggers was a 404 on every Hub whose
    // name is not lowercase.
    let Some(sensor) = portal_state
        .device(&hostname)
        .and_then(|d| d.sensors.get(line as usize))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let can_edit = render::can_edit_thresholds(&current_user);
    let mut rows = [render::sensor_card_ctx(&hostname, sensor, can_edit)];
    set_probe_calibration_flags(&state, &hostname, &mut rows);
    let [row] = rows;
    render_fragment(
        &state,
        "partials/hub_sensor_row.html",
        context! { s => row },
    )
}

/// The 1-Wire defaults a Hub runs with when its config omits a key
/// (storage_intervals.md §3, `fiber.config.yaml`): sample 2 s, aggregation
/// 600 s, three failed reads before a line counts as disconnected.
const DEFAULT_SAMPLE_INTERVAL_S: i64 = 2;
const DEFAULT_AGGREGATION_INTERVAL_S: i64 = 600;
const DEFAULT_FAILURE_THRESHOLD: i64 = 3;

/// The probe slug of `/hubs/{hub}/sensors/{probe}` (devices.hub.md §3.2,
/// amendment 2026-10-14): `line-{n}` with `n` 1-based like the UI, mapped to
/// the wire's 0..7. `internal-{name}` is reserved for the Hub's internal
/// sensors; the Hub publishes none today, so it resolves to nothing (404).
fn parse_probe_slug(slug: &str) -> Option<u8> {
    let n: u8 = slug.strip_prefix("line-")?.parse().ok()?;
    (1..=8).contains(&n).then(|| n - 1)
}

/// `GET /hubs/{hub}/sensors/{probe}` — one wired probe's page, no tabs:
/// the probe window (chart, statistics, events; monitoring.md §6, moved here
/// 2026-10-14), then limits, calibration, metadata, the Hub-wide intervals
/// and the line's diagnostics. Viewer-readable like the probe page; the
/// edits inside are manager+ through their existing routes.
pub(crate) async fn hub_sensor_page(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path((hub, probe)): Path<(String, String)>,
    Query(q): Query<super::probe_routes::WindowQuery>,
) -> Response {
    let portal_state = state.store.snapshot();
    let Some(hostname) = known_hub_in(&portal_state, &state.cluster, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    let (Some(line), Some(device)) = (parse_probe_slug(&probe), portal_state.device(&hostname))
    else {
        return not_found(&state, Some(&current_user));
    };
    let Some(sensor) = device.sensors.get(usize::from(line)) else {
        return not_found(&state, Some(&current_user));
    };
    let Some(w) =
        super::probe_routes::line_window(&state, &current_user, &device.hostname, line, &q).await
    else {
        return not_found(&state, Some(&current_user));
    };
    let can_edit = render::can_edit_thresholds(&current_user);
    let mut rows = [render::sensor_card_ctx(&device.hostname, sensor, can_edit)];
    set_probe_calibration_flags(&state, &device.hostname, &mut rows);
    let [s] = rows;

    // Hub-wide, read-only: what the Hub states in `config/state.intervals`,
    // else the default, labelled as such. The Hub does not publish
    // `failure_threshold` at all.
    let stated = |ms: Option<i64>| ms.map(|ms| ms / 1000);
    let interval = |value: Option<i64>, default: i64| {
        context! { value => value.unwrap_or(default), default => value.is_none() }
    };
    let intervals = context! {
        sample => interval(stated(device.config.sample_interval_ms), DEFAULT_SAMPLE_INTERVAL_S),
        aggregation => interval(
            stated(device.config.aggregation_interval_ms),
            DEFAULT_AGGREGATION_INTERVAL_S
        ),
        failure_threshold => interval(None, DEFAULT_FAILURE_THRESHOLD),
    };
    let meta = hub_meta(&state, &device.hostname).await;
    let cal = match super::calibration_routes::Target::probe(&state, &device.hostname, line) {
        Some(target) => {
            Some(super::calibration_routes::card_ctx(&state, &target, &current_user).await)
        }
        None => None,
    };
    let diag = context! {
        line => line + 1,
        sensor_type => "DS18B20",
        // The Hub publishes no ROM id for a line (SensorState has none).
        rom_id => Option::<String>::None,
        failures => sensor.disconnected_count,
        samples => sensor.sample_count,
        last_error => (!sensor.connected).then(|| s.dominant_alarm_state.clone()),
    };
    let probe_description = {
        let (h, e) = (device.hostname.clone(), probe_meta_entity(line));
        state
            .db
            .run(move |st| st.get_device_meta(&h, PROBE_META_SCOPE, &e))
            .await
            .ok()
            .flatten()
            .and_then(|m| m.description)
    };
    let hub_name = render::hub_display_name(Some(device), meta.as_ref());
    // The Channels card (ui_components.md §2) in place of the probe
    // window's chart and aggregate line.
    let channels = super::channels::probe_card(
        format!("channels-line-{line}"),
        &device.hostname,
        line,
        &s,
        render::now_unix(),
    );
    // "Device information" (metadata.cs.md §5): replaces the metadata card.
    let device_info = render::probe_info_ctx(
        render::ProbeInfoInputs {
            hostname: &device.hostname,
            hub_name: hub_name.as_deref().unwrap_or(&device.hostname),
            line,
            // The Hub publishes no ROM id for a line (SensorState has none).
            rom: None,
            sensor_type: Some("DS18B20".to_string()),
            updated_at: s.updated_at.clone(),
            name: s.name.clone(),
            location: s.location.clone(),
            description: probe_description.clone(),
        },
        cal.as_ref(),
        s.can_edit,
    );
    let ctx = super::probe_routes::window_page_ctx(
        &state,
        &current_user,
        w,
        &q,
        "hubs",
        context! {
            di => device_info,
            ch => channels,
            s => s,
            cal => cal,
            evc => super::probe_routes::events_card(&state, &current_user, &format!("probe:{line}"), Some(&device.hostname)).await,
            hub_name => hub_name,
            probe_description => probe_description,
            locations => super::location_suggestions(&state).await,
            intervals => intervals,
            diag => diag,
        },
    )
    .await;
    render_page(&state, "pages/hub_sensor.html", ctx)
}

/// Puts each line's calibration icon on its row: the cached levels, so a
/// row render stays one map lookup (calibration.cs.md §6 OQ 3, amendment
/// 2026-10-14).
fn set_probe_calibration_flags(
    state: &AppState,
    hostname: &str,
    rows: &mut [render::SensorCardCtx],
) {
    let levels = state
        .calibration
        .levels_for(hostname, crate::calibration::CalibrationScope::Probe);
    let expiries = state.calibration.probe_expiries(hostname);
    for row in rows {
        let line = row.line.to_string();
        row.calibration = render::calibration_flag(levels.get(&line).copied());
        row.calibration_until = expiries
            .get(&line)
            .map(|until| crate::calibration::rfc3339(*until));
    }
}

#[derive(Deserialize)]
struct SensorTextForm {
    #[serde(default)]
    value: String,
}

/// The wire is a plain string with no stated cap; 64 characters is the LCD
/// and list-cell reality (the same cap `device_label` gets in
/// `display-config.md`). Empty clears the field, which is how a line gets
/// back to its "Line N" fallback.
const SENSOR_TEXT_MAX: usize = 64;

/// Manager+ (§7): `set_sensor_name` for line 0..7.
async fn set_sensor_name(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path((hub, line)): Path<(String, u8)>,
    Form(form): Form<SensorTextForm>,
) -> Response {
    set_sensor_text(
        &state,
        &current_user,
        &hub,
        line,
        "set_sensor_name",
        "name",
        form.value,
    )
    .await
}

/// Manager+ (§7): `set_sensor_location` for line 0..7.
async fn set_sensor_location(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path((hub, line)): Path<(String, u8)>,
    Form(form): Form<SensorTextForm>,
) -> Response {
    set_sensor_text(
        &state,
        &current_user,
        &hub,
        line,
        "set_sensor_location",
        "location",
        form.value,
    )
    .await
}

#[derive(Deserialize)]
struct ProbeMetaForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    location: String,
}

/// `device_meta` scope of a wired probe's row (V39).
const PROBE_META_SCOPE: &str = "probe";

/// `device_meta` entity of a wired probe: its page slug, `line-N`, N 1..8.
pub(crate) fn probe_meta_entity(line: u8) -> String {
    format!("line-{}", u32::from(line) + 1)
}

/// Manager+: the probe's metadata card (metadata.cs.md). The description is
/// portal-side (`device_meta`, scope `probe`); name and location are the
/// Hub's own, so each is relayed as its signed command, and only when it
/// changed — one card save never sends a downlink-free no-op.
async fn set_probe_meta(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path((hub, line)): Path<(String, u8)>,
    Form(form): Form<ProbeMetaForm>,
) -> Response {
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    let Some(sensor) = (line <= 7)
        .then(|| state.store.snapshot_device(&hostname))
        .flatten()
        .and_then(|d| d.sensors.get(usize::from(line)).cloned())
    else {
        return not_found(&state, Some(&current_user));
    };
    let cmd = Cmd::new(&state, &current_user, "probe_meta_set");
    if form.description.trim().chars().count() > super::DESCRIPTION_MAX {
        return cmd.fail(&tr("monitoring.meta.err_description_too_long"));
    }
    let description = non_empty(form.description.clone());
    let entity = probe_meta_entity(line);
    let saved = super::edit_device_meta(
        &state,
        &current_user,
        &hostname,
        PROBE_META_SCOPE,
        &entity,
        |row| row.description = description.clone(),
    )
    .await;
    super::audit_action(
        &state,
        &current_user,
        "probe_meta_set",
        &format!("{hostname}/{entity}"),
        json!({ "description": description }),
        saved.is_ok(),
    )
    .await;
    if let Err(err) = &saved {
        tracing::warn!(%err, hostname, line, "failed to save probe metadata");
        return cmd.fail(&tr("monitoring.meta.err_save_failed"));
    }

    let changed =
        |new: &str, old: &Option<String>| new.trim() != old.as_deref().unwrap_or("").trim();
    match (
        changed(&form.name, &sensor.name),
        changed(&form.location, &sensor.location),
    ) {
        // Both: name, then location — two signed commands in a row, the
        // second only after the first succeeded (metadata.cs.md §5).
        (true, true) => {
            set_sensor_name_and_location(&state, &current_user, &hostname, line, &form).await
        }
        (true, false) => {
            set_sensor_text(
                &state,
                &current_user,
                &hub,
                line,
                "set_sensor_name",
                "name",
                form.name.clone(),
            )
            .await
        }
        (false, true) => {
            set_sensor_text(
                &state,
                &current_user,
                &hub,
                line,
                "set_sensor_location",
                "location",
                form.location.clone(),
            )
            .await
        }
        (false, false) => cmd.ok(&tr("monitoring.meta.saved")),
    }
}

/// One of the two per-line text commands as an outcome, audited like
/// [`set_sensor_text`]; a refusal before signing is the `Err` outcome.
async fn sensor_text_outcome(
    state: &AppState,
    current_user: &CurrentUser,
    hostname: &str,
    line: u8,
    command_type: &str,
    param: &str,
    value: &str,
) -> CommandOutcome {
    let cmd = Cmd::new(state, current_user, command_type);
    let value = value.trim();
    if value.chars().count() > SENSOR_TEXT_MAX {
        return cmd.error(&tr("monitoring.hubs.sensor_text_too_long"));
    }
    let params = json!({ "line": line, param: value });
    match cmd.execute(hostname, params.clone(), None).await {
        Ok(outcome) => {
            cmd.audit(
                &format!("sensor_{param}_set"),
                &format!("{hostname}/line-{line}"),
                params,
                &outcome,
            )
            .await;
            outcome
        }
        Err(refused) => refused,
    }
}

/// The "Device information" save when both name and location changed:
/// `set_sensor_name`, then — only if it succeeded — `set_sensor_location`.
/// The fragment names both commands and each one's status.
async fn set_sensor_name_and_location(
    state: &AppState,
    current_user: &CurrentUser,
    hostname: &str,
    line: u8,
    form: &ProbeMetaForm,
) -> Response {
    let name = sensor_text_outcome(
        state,
        current_user,
        hostname,
        line,
        "set_sensor_name",
        "name",
        &form.name,
    )
    .await;
    let label = |o: &CommandOutcome| {
        format!(
            "{}: {}{}",
            o.command_type,
            o.status,
            o.message
                .as_deref()
                .map(|m| format!(" ({m})"))
                .unwrap_or_default()
        )
    };
    let cmd = Cmd::new(state, current_user, "set_sensor_name");
    if !super::command::succeeded(&name) {
        let mut out = name;
        out.message = Some(format!(
            "{}; set_sensor_location: {}",
            label(&out),
            tr("monitoring.hub_sensor.location_not_sent")
        ));
        return cmd.done(out);
    }
    let location = sensor_text_outcome(
        state,
        current_user,
        hostname,
        line,
        "set_sensor_location",
        "location",
        &form.location,
    )
    .await;
    let summary = format!("{}; {}", label(&name), label(&location));
    let mut out = location;
    out.command_type = "set_sensor_name + set_sensor_location".to_string();
    out.message = Some(summary);
    cmd.done(out)
}

/// The two per-line text commands differ only in the command type and the
/// name of the one string parameter (`control-core` `monitor.rs`, the
/// `set_sensor_name`/`set_sensor_location` arms).
async fn set_sensor_text(
    state: &AppState,
    current_user: &CurrentUser,
    hub: &str,
    line: u8,
    command_type: &str,
    param: &str,
    value: String,
) -> Response {
    let Some(hostname) = known_hub(state, hub) else {
        return not_found(state, Some(current_user));
    };
    if line > 7 {
        return not_found(state, Some(current_user));
    }
    let value = value.trim();
    let cmd = Cmd::new(state, current_user, command_type);
    if value.chars().count() > SENSOR_TEXT_MAX {
        return cmd.fail(&tr("monitoring.hubs.sensor_text_too_long"));
    }
    Cmd::new(state, current_user, command_type)
        .relay(
            &hostname,
            json!({ "line": line, param: value }),
            None,
            &format!("sensor_{param}_set"),
            &format!("{hostname}/line-{line}"),
            json!({ "line": line, param: value }),
        )
        .await
}

// ---------------------------------------------------------------------
// POST /hubs/{hub}/system/info-interval
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct IntervalForm {
    #[serde(default)]
    interval_seconds: String,
}

#[derive(Deserialize)]
struct ReadIntervalForm {
    #[serde(default)]
    sample_interval_s: String,
}

/// Admin (config_sync.md §13 Q18): the 1-Wire read interval, `set_interval`. The verb
/// carries the whole triple, so the window and report interval are kept as they are —
/// the Portal's stored copy, else what the Hub reports. Saved in the Portal and pulled by
/// the Hub (sync document), never relayed.
async fn set_read_interval(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<ReadIntervalForm>,
) -> Response {
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    // Only the 1-Wire sample interval goes to the Hub; aggregation stays in
    // the Portal (config_sync.md §13 Q19). The range is fixed and checked
    // here, so the UI never says "Saved" for a value the Hub rejects.
    let cmd = Cmd::new(&state, &current_user, "set_sample_interval");
    let range = fiber_protocol::config_sync::SAMPLE_INTERVAL_S;
    let sample_s = match form.sample_interval_s.trim().parse::<u64>() {
        Ok(v) if range.contains(&v) => v,
        _ => {
            return cmd.fail(
                &tr("monitoring.hubs.read_interval_invalid")
                    .replace("{max}", &range.end().to_string()),
            );
        }
    };
    let params = json!({ "sample_interval_s": sample_s });
    cmd.dispatch_stored(
        &hostname,
        params,
        "read_interval_set",
        &hostname,
        json!({ "sample_interval_s": sample_s }),
    )
    .await
}

/// Bounds the agent does not enforce (`monitor.rs` clamps only to `max(1)`
/// when scheduling). 10 s is the floor below which `system/info` is most of
/// the broker traffic; 86 400 s is once a day, past which the liveness
/// picture the Overview cards paint is a day stale.
const INFO_INTERVAL_RANGE: std::ops::RangeInclusive<u64> = 10..=86_400;

/// Admin (§3.4, §7): `set_system_info_interval`, range-checked server-side.
async fn set_system_info_interval(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<IntervalForm>,
) -> Response {
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    let interval = match form.interval_seconds.trim().parse::<u64>() {
        Ok(v) if INFO_INTERVAL_RANGE.contains(&v) => v,
        _ => {
            return Cmd::new(&state, &current_user, "set_system_info_interval")
                .fail(&tr("monitoring.hubs.info_interval_invalid"));
        }
    };
    Cmd::new(&state, &current_user, "set_system_info_interval")
        .relay(
            &hostname,
            json!({ "interval_seconds": interval }),
            None,
            "system_info_interval_set",
            &hostname,
            json!({ "interval_seconds": interval }),
        )
        .await
}

// ---------------------------------------------------------------------
// POST /hubs/{hub}/display/{setting} and /display/sensors, the Unit tab
// (plan/portal/display-config.md §4/§5, placed on the `unit` tab by
// devices.hub.md §3.3)
// ---------------------------------------------------------------------

/// The one form field each scalar display control posts, kept as a raw
/// string so a blank or a non-number is rejected here (with a message the
/// operator sees) rather than by `Form`'s own deserializer.
#[derive(Deserialize)]
struct DisplayValueForm {
    #[serde(default)]
    value: String,
}

/// The four numeric settings of display-config.md §4's table, each with the
/// `command_type` it relays, the wire parameter name it fills and the audit
/// event it writes. The route's `{setting}` segment is matched against
/// `slug`; anything else is a 404, like any other unserved path.
struct DisplaySetting {
    slug: &'static str,
    command_type: &'static str,
    param: &'static str,
    audit_event: &'static str,
    /// Inclusive server-side range. The agent casts `as_u64() as u8`
    /// without clamping (`control-core` `monitor.rs`), so `volume: 300`
    /// would wrap to 44 — HTML `min`/`max` is UX only and this is the
    /// check that counts (display-config.md §3 warning).
    range: std::ops::RangeInclusive<u64>,
}

const DISPLAY_SETTINGS: [DisplaySetting; 4] = [
    DisplaySetting {
        slug: "buzzer-volume",
        command_type: "set_buzzer_volume",
        param: "volume",
        audit_event: "display_buzzer_volume_set",
        range: 0..=100,
    },
    DisplaySetting {
        slug: "screen-brightness",
        command_type: "set_screen_brightness",
        param: "brightness",
        audit_event: "display_screen_brightness_set",
        range: 0..=100,
    },
    DisplaySetting {
        slug: "led-brightness",
        command_type: "set_led_brightness",
        param: "brightness",
        audit_event: "display_led_brightness_set",
        range: 0..=100,
    },
    DisplaySetting {
        slug: "screen-timeout",
        command_type: "set_screen_timeout",
        param: "timeout_secs",
        audit_event: "display_screen_timeout_set",
        // 0 = always on; 86 400 s is a day, past which an "idle timeout"
        // has stopped meaning anything (display-config.md §4).
        range: 0..=86_400,
    },
];

/// Longest `device_label` the LCD and the list cells can carry — the same
/// 64 characters [`SENSOR_TEXT_MAX`] gives a line name.
const DEVICE_LABEL_MAX: usize = 64;

/// Manager+ (devices.hub.md §3.3/§7, settling display-config.md's OQ 2):
/// the four numeric display settings, `POST /hubs/{hub}/display/{setting}`.
/// Out of range is answered in `#command-status` and relays nothing.
async fn set_display_scalar(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path((hub, setting)): Path<(String, String)>,
    Form(form): Form<DisplayValueForm>,
) -> Response {
    let Some(spec) = DISPLAY_SETTINGS.iter().find(|s| s.slug == setting) else {
        return not_found(&state, Some(&current_user));
    };
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };

    let value = match form.value.trim().parse::<u64>() {
        Ok(v) if spec.range.contains(&v) => v,
        _ => {
            let message = tr("monitoring.hubs.display_value_out_of_range")
                .replace("{min}", &spec.range.start().to_string())
                .replace("{max}", &spec.range.end().to_string());
            return Cmd::new(&state, &current_user, spec.command_type).fail(&message);
        }
    };

    Cmd::new(&state, &current_user, spec.command_type)
        .relay(
            &hostname,
            json!({ spec.param: value }),
            None,
            spec.audit_event,
            &hostname,
            json!({ spec.param: value }),
        )
        .await
}

/// Manager+: `POST /hubs/{hub}/display/device-label`, the name the LCD
/// shows. Its own handler because the value is a string, not a range.
async fn set_device_label(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<DisplayValueForm>,
) -> Response {
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };
    let label = form.value.trim();
    if label.is_empty() || label.chars().count() > DEVICE_LABEL_MAX {
        return Cmd::new(&state, &current_user, "set_device_label")
            .fail(&tr("monitoring.hubs.device_label_invalid"));
    }
    Cmd::new(&state, &current_user, "set_device_label")
        .relay(
            &hostname,
            json!({ "label": label }),
            None,
            "display_device_label_set",
            &hostname,
            json!({ "label": label }),
        )
        .await
}

/// The picker's repeated `sensors` field — a checkbox list posts the id of
/// every checked box under one name, which `serde_urlencoded` cannot
/// collect into a `Vec`; `axum_extra`'s `Form` (serde_html_form) can, the
/// same reason the bulk alarm-acknowledge form uses it.
#[derive(Deserialize)]
struct SensorPickForm {
    #[serde(default)]
    sensors: Vec<String>,
}

/// Manager+: `POST /hubs/{hub}/display/sensors` (display-config.md §5,
/// DC-3/DC-4/DC-5). Resolves each opaque catalogue id to a `DisplayLine`,
/// caps the set at 8 server-side, validates the list and relays
/// `set_display_lines`. An empty selection is `[]`, which restores the
/// built-in layout.
///
/// `relay_signed`, like every other Hub command. The portal's own
/// certificate preflight is universal since P4 (plan/portal/mqtt/design.md
/// §4.4 step 3) and runs before signing here as everywhere; whatever the
/// device checks on top of it is the device's business.
async fn set_display_sensors(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    axum_extra::extract::Form(form): axum_extra::extract::Form<SensorPickForm>,
) -> Response {
    let Some(hostname) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&current_user));
    };

    let ids: Vec<String> = form.sensors.into_iter().filter_map(non_empty).collect();

    if ids.len() > crate::display_catalog::MAX_SELECTED {
        let message = tr("monitoring.hubs.display_too_many_sensors")
            .replace("{max}", &crate::display_catalog::MAX_SELECTED.to_string());
        return Cmd::new(&state, &current_user, "set_display_lines").fail(&message);
    }

    let mut lines = Vec::with_capacity(ids.len());
    for id in &ids {
        let Some(line) = crate::display_catalog::resolve(id) else {
            return Cmd::new(&state, &current_user, "set_display_lines")
                .fail(&tr("monitoring.hubs.display_unknown_sensor").replace("{id}", id));
        };
        lines.push(line);
    }

    if let Err(err) = fiber_protocol::validate_display_custom_lines(&lines) {
        return Cmd::new(&state, &current_user, "set_display_lines").fail(&err);
    }

    Cmd::new(&state, &current_user, "set_display_lines")
        .relay(
            &hostname,
            json!({ "lines": lines }),
            None,
            "display_lines_set",
            &hostname,
            json!({ "sensors": ids }),
        )
        .await
}

/// `GET /hubs/{hub}/display/picker`: the picker alone, refetched by itself
/// on the `device-{hostname}-status` nudge — the event `ChangeEvent::Config`
/// debounces into (`src/sse.rs`), so the device's `config/state` echo is
/// what re-ticks the boxes.
async fn display_picker(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path(hub): Path<String>,
) -> Response {
    let portal_state = state.store.snapshot();
    let Some(hostname) = known_hub_in(&portal_state, &state.cluster, &hub) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(device) = portal_state.device(&hostname) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let catalog = crate::display_catalog::catalog(device);
    let paired = device_is_paired(&state, &hostname).await;
    render_fragment(
        &state,
        "partials/hub_display_picker.html",
        context! {
            hostname => hostname,
            pairing => context! { paired => paired },
            can_manage => render::can_edit_thresholds(&current_user),
            sensor_catalog => catalog,
            max_selected => crate::display_catalog::MAX_SELECTED,
        },
    )
}

/// Is this Hub paired? The fragment route has no page context to inherit
/// the answer from, and an unpaired Hub must come back with its controls
/// still disabled.
async fn device_is_paired(state: &AppState, hostname: &str) -> bool {
    matches!(state.keys.device_identity(hostname).await, Ok(Some(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_origin_keeps_the_words_the_agent_sends() {
        assert_eq!(sanitize_origin(Some("ble")), "ble");
        assert_eq!(sanitize_origin(Some("signed-mqtt")), "signed-mqtt");
        assert_eq!(sanitize_origin(Some("cli")), "cli");
    }

    #[test]
    fn sanitize_origin_refuses_to_pass_wire_input_into_a_stored_row() {
        // The value reaches an audit row and a log line, both read by an operator later. A
        // quote-and-brace payload must not survive into either, and an absent or hostile one
        // still has to name something.
        assert_eq!(
            sanitize_origin(Some(r#"x","error":"forged"#)),
            "xerrorforged"
        );
        assert_eq!(sanitize_origin(Some("")), "device");
        assert_eq!(sanitize_origin(None), "device");
        assert_eq!(sanitize_origin(Some("\n\t  ")), "device");
        assert_eq!(sanitize_origin(Some(&"a".repeat(100))).len(), 32, "bounded");
    }

    #[test]
    fn a_pairing_code_off_the_wire_is_normalised_the_way_the_front_panel_writes_it() {
        // Lowercase off a phone keyboard and a trailing newline off a BLE client are both the
        // operator getting it right (FB1A's own `normalize` makes the same two allowances).
        assert_eq!(
            normalize_pairing_code(Some("23456X")).as_deref(),
            Some("23456X")
        );
        assert_eq!(
            normalize_pairing_code(Some("23456x")).as_deref(),
            Some("23456X")
        );
        assert_eq!(
            normalize_pairing_code(Some(" 23456X\n")).as_deref(),
            Some("23456X")
        );
    }

    #[test]
    fn nothing_that_is_not_a_pairing_code_gets_past_the_shape_check() {
        // The five characters the alphabet drops, because an operator reads the code off one
        // screen and types it into another.
        for ambiguous in ["2345O0", "2345I1", "23456L"] {
            assert_eq!(normalize_pairing_code(Some(ambiguous)), None, "{ambiguous}");
        }
        // And the shapes that cost the operator a pairing window if they reach the device: an
        // empty payload, a short code, a long one, punctuation.
        for bad in ["", "   ", "ABC", "ABCDEFG", "23-45X", "2345 X"] {
            assert_eq!(normalize_pairing_code(Some(bad)), None, "{bad:?}");
        }
        assert_eq!(normalize_pairing_code(None), None);
    }

    #[test]
    fn pubkey_matches_true_for_derived_key() {
        let seed = [7u8; 32];
        let derived = fiber_protocol::derive_public_hex(&seed);
        assert!(pubkey_matches(&seed, &derived));
    }

    #[test]
    fn pubkey_matches_false_for_mismatched_key() {
        let seed = [7u8; 32];
        let other = fiber_protocol::derive_public_hex(&[9u8; 32]);
        assert!(!pubkey_matches(&seed, &other));
    }

    #[test]
    fn is_local_device_matches_across_the_hubs_uppercasing() {
        // The hub uppercases /etc/hostname for its MQTT identity; the portal
        // reads the raw name from gethostname(2). Same machine, different case.
        assert!(is_local_device("FIBER-001", "fiber-001"));
        assert!(is_local_device("fiber-001", "FIBER-001"));
        assert!(is_local_device("FIBER-001", "FIBER-001"));
    }

    #[test]
    fn is_local_device_ignores_surrounding_whitespace() {
        assert!(is_local_device(" FIBER-001 ", "fiber-001\n"));
    }

    /// Only the portal's own Hub is flagged, and only while the seam is
    /// inert: another Hub's radio is not this portal's to vouch for.
    #[test]
    fn radio_control_off_flags_only_the_local_hub_with_an_inert_seam() {
        assert!(local_radio_control_off(
            "PROXIMOS-0001",
            "proximos-0001",
            false
        ));
        assert!(!local_radio_control_off(
            "PROXIMOS-0001",
            "proximos-0001",
            true
        ));
        assert!(!local_radio_control_off(
            "PROXIMOS-0002",
            "proximos-0001",
            false
        ));
        assert!(!local_radio_control_off("PROXIMOS-0001", "", false));
    }

    #[test]
    fn is_local_device_false_for_another_hub() {
        assert!(!is_local_device("FIBER-002", "FIBER-001"));
    }

    #[test]
    fn is_local_device_false_when_either_side_is_blank() {
        // An unreadable portal hostname must degrade to the code flow, never
        // to "everything is local".
        assert!(!is_local_device("FIBER-001", ""));
        assert!(!is_local_device("", "FIBER-001"));
        assert!(!is_local_device("   ", "  "));
    }

    fn valid_form() -> FactoryResetForm {
        FactoryResetForm {
            reason: "shutting down for maintenance".to_string(),
            confirm_hostname: "FIBER-001".to_string(),
            post_action: "reboot".to_string(),
        }
    }

    #[test]
    fn factory_reset_valid_form_passes() {
        assert!(validate_factory_reset(&valid_form(), Some("FIBER-001")).is_ok());
    }

    #[test]
    fn factory_reset_rejects_short_reason() {
        let mut form = valid_form();
        form.reason = "short".to_string();
        let err = validate_factory_reset(&form, Some("FIBER-001")).unwrap_err();
        assert!(err.contains("8 non-whitespace"));
    }

    #[test]
    fn factory_reset_rejects_whitespace_only_padding_as_short() {
        // 10 chars but only 4 non-whitespace — must still fail the
        // non-whitespace count, not the raw length.
        let mut form = valid_form();
        form.reason = "a  b  c   ".to_string();
        assert!(validate_factory_reset(&form, Some("FIBER-001")).is_err());
    }

    #[test]
    fn factory_reset_rejects_reason_over_256_chars() {
        let mut form = valid_form();
        form.reason = "a".repeat(257);
        let err = validate_factory_reset(&form, Some("FIBER-001")).unwrap_err();
        assert!(err.contains("256"));
    }

    #[test]
    fn factory_reset_rejects_hostname_mismatch() {
        let mut form = valid_form();
        form.confirm_hostname = "WRONG-HOST".to_string();
        let err = validate_factory_reset(&form, Some("FIBER-001")).unwrap_err();
        assert!(err.contains("does not match"));
    }

    #[test]
    fn factory_reset_rejects_no_device() {
        let err = validate_factory_reset(&valid_form(), None).unwrap_err();
        assert!(err.contains("No device"));
    }

    #[test]
    fn factory_reset_rejects_bad_post_action() {
        let mut form = valid_form();
        form.post_action = "delete_everything".to_string();
        let err = validate_factory_reset(&form, Some("FIBER-001")).unwrap_err();
        assert!(err.contains("post_action"));
    }

    #[test]
    fn err_message_has_a_fallback_for_unknown_codes() {
        assert!(err_message("something-nobody-sends").is_some());
    }

    #[test]
    fn ok_message_unknown_code_is_none() {
        assert!(ok_message("something-nobody-sends").is_none());
    }

    #[test]
    fn blank_threshold_maps_to_none() {
        assert_eq!(parse_optional_float("", "x").unwrap(), None);
        assert_eq!(parse_optional_float("   ", "x").unwrap(), None);
    }

    #[test]
    fn numeric_threshold_string_parses() {
        assert_eq!(parse_optional_float("-18.5", "x").unwrap(), Some(-18.5));
        assert_eq!(parse_optional_float("0", "x").unwrap(), Some(0.0));
    }

    #[test]
    fn non_numeric_threshold_string_errors() {
        assert!(parse_optional_float("abc", "critical_low").is_err());
    }

    #[test]
    fn each_system_command_audits_under_its_own_name() {
        assert_eq!(audit_event("restart_application"), "device_restart");
        assert_eq!(audit_event("power_off"), "device_power_off");
        assert_eq!(audit_event("factory_reset"), "device_factory_reset");
    }
}

/// Route-level cover for the family's contract: the redirects, the 404 for
/// anything the portal does not know, and the local-vs-remote pairing fork
/// (whether the code gate applies at all is decided per device, and the
/// form field it guards is simply absent from the page for a local hub).
#[cfg(test)]
mod route_tests {
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use fiber_protocol::payloads::status::DeviceStatus;
    use tower::ServiceExt;

    use serde_json::json;

    use crate::auth::{Role, SESSION_COOKIE};
    use crate::routes::test_support::{login, login_as_role, test_state};
    use crate::routes::tr;
    use crate::state::IngestEvent;

    /// `(router, tempdir guard, admin session)` with one online Hub "hub1".
    /// `local_hostname` is what the portal believes its own machine is
    /// called — pass `Some("hub1")` to make that device local.
    async fn hub_app(
        local_hostname: Option<&str>,
    ) -> (
        axum::Router,
        tempfile::TempDir,
        String,
        crate::http::AppState,
    ) {
        let (mut state, dir) = test_state().await;
        if let Some(name) = local_hostname {
            state.local_hostname = name.to_string();
        }
        state.store.apply(IngestEvent::Status {
            hostname: "hub1".to_string(),
            status: DeviceStatus::Online,
        });
        let token = login(&state, "admin", "correct-horse-battery").await;
        let router = super::router()
            .merge(crate::routes::probe_routes::router())
            .with_state(state.clone());
        (router, dir, token, state)
    }

    async fn get(app: &axum::Router, uri: &str, token: &str) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
    }

    async fn post(
        app: &axum::Router,
        uri: &str,
        token: &str,
        body: &'static str,
    ) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .expect("request"),
            )
            .await
            .expect("response")
    }

    fn location(response: &axum::response::Response) -> String {
        response
            .headers()
            .get(header::LOCATION)
            .expect("location")
            .to_str()
            .expect("ascii location")
            .to_string()
    }

    async fn body_of(response: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        String::from_utf8_lossy(&bytes).to_string()
    }

    /// HS-1: the sensor row fragment resolves a Hub whose name is not lowercase — the row
    /// fetch every SSE sensor nudge triggers was a 404 on such a Hub.
    #[tokio::test]
    async fn the_sensor_row_resolves_an_uppercase_hub() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        state.store.apply(IngestEvent::Status {
            hostname: "FIBER-4E970F4A".to_string(),
            status: DeviceStatus::Online,
        });
        let response = get(&app, "/hubs/FIBER-4E970F4A/sensors/0/row", &token).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// External gateways (backend preparation, 2026-09-26): admin only, the Portal's own Hub
    /// only, a 16-hex EUI; the radio seam's answer (here: no radio config) is what comes back.
    #[tokio::test]
    async fn the_gateway_routes_are_admin_only_and_local() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let body = "gateway_eui=0016c001f1500001&name=Hala";
        let denied = post(&app, "/hubs/hub1/radio/gateways", &manager, body).await;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);

        let bad =
            body_of(post(&app, "/hubs/hub1/radio/gateways", &token, "gateway_eui=xyz").await).await;
        assert!(bad.contains("16 hex digits"), "{bad}");

        let seam = body_of(post(&app, "/hubs/hub1/radio/gateways", &token, body).await).await;
        assert!(seam.contains("no rendered radio config"), "{seam}");

        let (remote, _d2, remote_token, _s2) = hub_app(Some("other-host")).await;
        let not_local = body_of(
            post(
                &remote,
                "/hubs/hub1/radio/gateways/remove",
                &remote_token,
                body,
            )
            .await,
        )
        .await;
        assert!(not_local.contains("own Hub only"), "{not_local}");
    }

    #[tokio::test]
    async fn a_tabless_hub_url_redirects_to_overview() {
        let (app, _dir, token, _state) = hub_app(Some("hub1")).await;
        let response = get(&app, "/hubs/hub1", &token).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&response), "/hubs/hub1/overview");
    }

    /// The old address keeps working, including its one deep link.
    #[tokio::test]
    async fn the_device_page_redirects_into_the_family() {
        let (app, _dir, token, _state) = hub_app(Some("hub1")).await;

        let response = get(&app, "/device", &token).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&response), "/hubs/hub1/overview");

        let response = get(&app, "/device?tab=configuration", &token).await;
        assert_eq!(location(&response), "/hubs/hub1/system");
    }

    /// With nothing in the store there is no Hub to scope a detail page to,
    /// so the list — which renders the waiting state — is the answer.
    #[tokio::test]
    async fn the_device_page_falls_back_to_the_list_before_any_hub_reports() {
        let (state, _dir) = test_state().await;
        let token = login(&state, "admin", "correct-horse-battery").await;
        let app = super::router().with_state(state.clone());

        let response = get(&app, "/device", &token).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&response), "/hubs");

        // And the list is what says so (§2.5), rather than an empty table.
        let response = get(&app, "/hubs", &token).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!body_of(response).await.contains("hub-rows"));
    }

    #[tokio::test]
    async fn an_unknown_hub_and_an_unknown_tab_both_404() {
        let (app, _dir, token, _state) = hub_app(Some("hub1")).await;

        for uri in [
            "/hubs/nope/overview",
            "/hubs/nope",
            "/hubs/hub1/configuration",
        ] {
            let response = get(&app, uri, &token).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn every_tab_renders_for_a_known_hub() {
        let (app, _dir, token, _state) = hub_app(Some("hub1")).await;
        for tab in ["overview", "sensors", "unit", "system"] {
            let response = get(&app, &format!("/hubs/hub1/{tab}"), &token).await;
            assert_eq!(response.status(), StatusCode::OK, "{tab}");
        }
    }

    /// UX-059: a Hub whose alarms e-mail nobody says so on its overview and
    /// links to where that is set; one with active recipients, its own or
    /// the defaults (#31), does not.
    #[tokio::test]
    async fn the_overview_warns_when_no_one_gets_the_hubs_alarm_emails() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        let link = "/settings/notifications?hub=hub1";
        let overview =
            |app, token| async move { body_of(get(app, "/hubs/hub1/overview", token).await).await };

        assert!(overview(&app, &token).await.contains(link), "no config row");
        // #31: the default recipients cover a Hub without a row of its own,
        // and only such a Hub: the rows below override them.
        state
            .db
            .run(|s| s.set_default_alarm_recipients(&["duty@example.com".to_string()]))
            .await
            .unwrap();
        assert!(!overview(&app, &token).await.contains(link), "defaults");

        let save = |enabled: bool, recipients: &[&str]| {
            let cfg = portal_storage::DeviceReportConfig {
                hostname: "hub1".into(),
                recipients: recipients.iter().map(|r| (*r).to_string()).collect(),
                enabled,
                notify_on_alarm: true,
                notify_on_clear: false,
                notify_on_ack: false,
                updated_at: None,
            };
            let db = state.db.clone();
            async move {
                db.run(move |s| s.upsert_device_report_config(&cfg))
                    .await
                    .unwrap()
            }
        };
        save(false, &["ops@example.com"]).await;
        assert!(overview(&app, &token).await.contains(link), "disabled");
        save(true, &[]).await;
        assert!(overview(&app, &token).await.contains(link), "no recipients");
        save(true, &["ops@example.com"]).await;
        assert!(!overview(&app, &token).await.contains(link), "configured");
    }

    /// The gate §3.3 tightens: a manager may no longer restart a Hub.
    #[tokio::test]
    async fn restart_is_admin_only_now() {
        let (app, _dir, _admin, state) = hub_app(Some("hub1")).await;
        let manager = login_as_role(&state, Role::Manager, "manager1").await;

        let response = post(&app, "/hubs/hub1/system/restart", &manager, "reason=").await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// §7: restart writes an audit row, which it did not before. The Hub has
    /// to be paired for the command to reach the relay at all, and nothing
    /// answers it — the row records the outcome, whatever it is.
    #[tokio::test(start_paused = true)]
    async fn restart_writes_an_audit_row() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        state
            .keys
            .store_pairing(
                "hub1",
                Some("test-ca".into()),
                "00".repeat(32),
                [7u8; 32],
                None,
                None,
            )
            .await
            .expect("store pairing");

        let response = post(&app, "/hubs/hub1/system/restart", &token, "reason=").await;
        assert_eq!(response.status(), StatusCode::OK);

        let (rows, _total) = state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some("device_restart".to_string()),
                ..Default::default()
            })
            .await
            .expect("audit page");
        assert_eq!(rows.len(), 1, "one device_restart row: {rows:?}");
        assert_eq!(rows[0].target.as_deref(), Some("hub1"));
    }

    /// A signed command goes to the Hub the PATH names (§8.2 rule 2), so an
    /// unknown one is refused rather than quietly sent to the first device.
    #[tokio::test]
    async fn a_command_for_an_unknown_hub_is_not_relayed() {
        let (app, _dir, token, _state) = hub_app(Some("hub1")).await;
        let response = post(
            &app,
            "/hubs/somewhere-else/system/restart",
            &token,
            "reason=",
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// Time is paused so the relay's 30 s pair timeout fires the moment the
    /// runtime idles: reaching `err=timeout` (with no device answering) is
    /// exactly the proof that the request got past the code gate.
    #[tokio::test(start_paused = true)]
    async fn local_pairing_accepts_a_form_with_no_code_field_at_all() {
        let (app, _dir, token, _state) = hub_app(Some("hub1")).await;
        let response = post(&app, "/hubs/hub1/pair", &token, "").await;
        assert_eq!(
            location(&response),
            "/hubs/hub1/system?err=timeout",
            "a local hub must not be asked for a pairing code"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn local_pairing_is_case_insensitive_about_the_hostname() {
        // The hub uppercases its MQTT hostname; gethostname(2) does not.
        let (app, _dir, token, _state) = hub_app(Some("HUB1")).await;
        assert_eq!(
            location(&post(&app, "/hubs/hub1/pair", &token, "").await),
            "/hubs/hub1/system?err=timeout"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn remote_pairing_still_demands_a_six_character_code() {
        let (app, _dir, token, _state) = hub_app(Some("some-other-host")).await;

        assert_eq!(
            location(&post(&app, "/hubs/hub1/pair", &token, "").await),
            "/hubs/hub1/system?err=invalid_code"
        );
        assert_eq!(
            location(&post(&app, "/hubs/hub1/pair", &token, "pairing_code=ABC").await),
            "/hubs/hub1/system?err=invalid_code"
        );
        assert_eq!(
            location(&post(&app, "/hubs/hub1/pair", &token, "pairing_code=ABC123").await),
            "/hubs/hub1/system?err=timeout",
            "a well-formed code goes on to the device"
        );
    }

    #[test]
    fn return_to_is_honoured_only_as_a_path_on_this_portal() {
        assert_eq!(
            super::safe_return_to(Some("/beacons/add")),
            Some("/beacons/add")
        );
        assert_eq!(
            super::safe_return_to(Some("/nodes/abc?x=1")),
            Some("/nodes/abc?x=1")
        );
        for bad in [
            None,
            Some(""),
            Some("beacons"),
            Some("//evil.example/"),
            Some("/\\evil.example/"),
            Some("https://evil.example/"),
            Some("/a\nLocation: x"),
        ] {
            assert_eq!(super::safe_return_to(bad), None, "{bad:?}");
        }
    }

    /// A refusal goes to the System tab even from a banner elsewhere: that is where the
    /// failure is explained and the same form waits.
    #[tokio::test(start_paused = true)]
    async fn a_failed_pairing_from_a_banner_still_lands_on_the_system_tab() {
        let (app, _dir, token, _state) = hub_app(Some("hub1")).await;
        assert_eq!(
            location(
                &post(
                    &app,
                    "/hubs/hub1/pair",
                    &token,
                    "return_to=%2Fbeacons%2Fadd"
                )
                .await
            ),
            "/hubs/hub1/system?err=timeout"
        );
    }

    /// #109, dodatek 2026-10-13: an admin pairs from the banner on any other tab and comes back
    /// to it; on the System tab the card is the form, so the banner carries none.
    #[tokio::test]
    async fn the_banner_on_another_tab_carries_the_pairing_form_back_to_that_tab() {
        let (app, _dir, token, _state) = hub_app(Some("hub1")).await;

        let html = body_of(get(&app, "/hubs/hub1/overview", &token).await).await;
        assert!(html.contains(r#"data-unpaired-hub="hub1""#), "{html}");
        assert!(html.contains(r#"data-pair-form="hub1""#), "{html}");
        // #155: the banner says how to open pairing mode, quoting the Hub display's menu label.
        assert!(html.contains("data-pairing-steps"), "{html}");
        assert!(html.contains("“Pairing mode”"), "{html}");
        assert!(
            html.contains(r#"name="return_to" value="&#x2f;hubs&#x2f;hub1&#x2f;overview""#),
            "{html}"
        );

        let html = body_of(get(&app, "/hubs/hub1/system", &token).await).await;
        assert_eq!(
            html.matches(r#"data-pair-form="hub1""#).count(),
            1,
            "only the card's form on the System tab: {html}"
        );
        assert!(!html.contains(r#"name="return_to""#), "{html}");
    }

    #[tokio::test]
    async fn the_system_tab_drops_the_code_input_only_for_the_local_hub() {
        for (local_hostname, expect_input) in [(Some("hub1"), false), (Some("elsewhere"), true)] {
            let (app, _dir, token, _state) = hub_app(local_hostname).await;
            let html = body_of(get(&app, "/hubs/hub1/system", &token).await).await;

            assert_eq!(
                html.contains(r#"name="pairing_code""#),
                expect_input,
                "local_hostname={local_hostname:?}"
            );
            assert!(
                html.contains(r#"action="/hubs/hub1/pair""#),
                "the pair form itself is there either way"
            );
        }
    }

    // ----- `pair/invite` (GitLab #61, plan/control/ble/peripheral.md §6.5) -----

    /// A `pair/invite` as the agent publishes it, in the shape `mqtt::dispatch` hands to the
    /// sink. `origin` is always `ble` here — what it carries is `sanitize_origin`'s business,
    /// tested there.
    fn invite(hostname: &str, code: Option<&str>) -> IngestEvent {
        IngestEvent::PairInvite {
            hostname: hostname.to_string(),
            payload: fiber_protocol::payloads::pair_invite::PairInviteMsg {
                code: code.map(str::to_string),
                origin: Some("ble".to_string()),
            },
        }
    }

    /// Runs `serve_pair_invites` over exactly `events` and returns once the last one has been
    /// taken off the channel.
    ///
    /// Every refusal is decided inline, so a refusal has happened by the time this returns. An
    /// *accepted* invite runs as its own task — that is the point of the spawn — so what it
    /// leaves behind is waited for with [`wait_for_audit`] / [`wait_for_publishes`].
    async fn feed_invites(state: &crate::http::AppState, events: Vec<IngestEvent>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for event in events {
            tx.send(event).expect("invite channel open");
        }
        drop(tx);
        super::serve_pair_invites(state.clone(), rx).await;
    }

    async fn audit_rows(
        state: &crate::http::AppState,
        event_type: &str,
    ) -> Vec<portal_storage::AuditEntry> {
        state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some(event_type.to_string()),
                // `list_audit` clamps the limit to at least 1, so the default 0 is one row —
                // which for a test counting refusals is every assertion passing for free.
                limit: 50,
                ..Default::default()
            })
            .await
            .expect("audit page")
            .0
    }

    /// The audit log once it holds at least `n` rows of `event_type`.
    ///
    /// An accepted invite has no handle to await, so the row is the observable. Time is paused
    /// in these tests, which is also what walks the relay's 30 s pair timeout forward; the loop
    /// is bounded so a row that never comes fails the test instead of hanging it.
    async fn wait_for_audit(
        state: &crate::http::AppState,
        event_type: &str,
        n: usize,
    ) -> Vec<portal_storage::AuditEntry> {
        // One virtual second per turn, generously past the relay's 30 s `PAIR_TIMEOUT` — the
        // clock only moves when everything else is idle, so this costs no real time.
        for _ in 0..120 {
            let rows = audit_rows(state, event_type).await;
            if rows.len() >= n {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        panic!("fewer than {n} {event_type} rows after waiting");
    }

    /// Everything the relay put on the wire, once there are at least `n` frames.
    async fn wait_for_publishes(
        state: &crate::http::AppState,
        n: usize,
    ) -> Vec<(String, serde_json::Value)> {
        for _ in 0..120 {
            let published = state.relay.published();
            if published.len() >= n {
                return published;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        panic!("fewer than {n} published frames after waiting");
    }

    /// The gate the POST route has had all along, on the path that does not go through the POST
    /// route. It is not about a pairing failing — `run_pairing` publishes `pair/request`, and
    /// the Hub spends the window on its display whether the code is right or not. A publisher
    /// sending `{}` on a loop would make pairing impossible.
    #[tokio::test(start_paused = true)]
    async fn an_invite_whose_code_is_not_a_pairing_code_is_refused_before_the_device_is_asked() {
        let (_app, _dir, _token, state) = hub_app(Some("somewhere-else")).await;

        let shapes = [None, Some(""), Some("ABC"), Some("AB0DEF"), Some(";;;;;;")];
        feed_invites(&state, shapes.iter().map(|c| invite("hub1", *c)).collect()).await;

        assert!(
            state.relay.published().is_empty(),
            "not one of these may reach the device: {:?}",
            state.relay.published()
        );

        let rows = audit_rows(&state, "device_pair").await;
        assert_eq!(rows.len(), shapes.len(), "one row per refusal: {rows:?}");
        for row in &rows {
            assert!(!row.success);
            assert_eq!(row.username, None, "nobody signed in for this");
            assert_eq!(row.user_id, None);
            assert_eq!(row.target.as_deref(), Some("hub1"));
            let details: serde_json::Value =
                serde_json::from_str(row.details.as_deref().expect("details")).expect("json");
            assert_eq!(details["error"], "invalid_code");
            assert_eq!(details["origin"], "ble");
        }
    }

    /// The hostname is a raw topic segment, so anything that can publish under the prefix can
    /// name anything. Unresolved, each unknown name cost the full 30 s pair timeout.
    #[tokio::test(start_paused = true)]
    async fn an_invite_for_a_hostname_this_portal_has_never_heard_from_is_refused() {
        let (_app, _dir, _token, state) = hub_app(Some("somewhere-else")).await;

        feed_invites(&state, vec![invite("hub-nobody-has-seen", Some("23456X"))]).await;

        assert!(
            state.relay.published().is_empty(),
            "an unknown hostname never reaches the relay"
        );
        let rows = audit_rows(&state, "device_pair").await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(!rows[0].success);
        assert_eq!(rows[0].target.as_deref(), Some("hub-nobody-has-seen"));
        let details: serde_json::Value =
            serde_json::from_str(rows[0].details.as_deref().expect("details")).expect("json");
        assert_eq!(details["error"], "unknown_hub");
    }

    /// The same payload, twice, differing only in which machine the portal runs on — because
    /// the message has no say in it. A payload that could claim locality could claim the fixed
    /// local password, so locality is recomputed from the hostname the topic named.
    #[tokio::test(start_paused = true)]
    async fn an_invites_locality_is_recomputed_from_the_hostname_not_taken_from_the_payload() {
        // The portal runs on hub1: no code is needed, and the codeless invite goes through.
        let (_app, _dir, _token, state) = hub_app(Some("hub1")).await;
        feed_invites(&state, vec![invite("hub1", None)]).await;

        let published = wait_for_publishes(&state, 1).await;
        assert_eq!(published.len(), 1, "{published:?}");
        assert_eq!(published[0].0, "proximos/hub1/pair/request");
        assert_eq!(
            published[0].1["local"], true,
            "the fixed local password is chosen here, never by the message"
        );
        assert_eq!(
            published[0].1["admin_username"], "invite:ble",
            "no portal user signed in, so the origin is what goes on the wire"
        );

        // The very same payload for a Hub the portal does not run on has no code to pair with.
        let (_app, _dir, _token, remote) = hub_app(Some("somewhere-else")).await;
        feed_invites(&remote, vec![invite("hub1", None)]).await;

        assert!(
            remote.relay.published().is_empty(),
            "a remote Hub needs the code the operator read off its display"
        );
        let rows = audit_rows(&remote, "device_pair").await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        let details: serde_json::Value =
            serde_json::from_str(rows[0].details.as_deref().expect("details")).expect("json");
        assert_eq!(details["error"], "invalid_code");
    }

    /// Nobody is signed in for an invited pairing, and the row has to say so rather than
    /// borrow an account. The failure is the relay's 30 s timeout — no device answers a test
    /// broker — which is also the proof the code gate let this one through.
    #[tokio::test(start_paused = true)]
    async fn a_failed_invited_pairing_writes_a_row_that_names_no_user() {
        let (_app, _dir, _token, state) = hub_app(Some("somewhere-else")).await;

        feed_invites(&state, vec![invite("hub1", Some("23456x"))]).await;

        let rows = wait_for_audit(&state, "device_pair", 1).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(!rows[0].success);
        assert_eq!(rows[0].username, None);
        assert_eq!(rows[0].user_id, None);
        assert_eq!(rows[0].target.as_deref(), Some("hub1"));
        let details: serde_json::Value =
            serde_json::from_str(rows[0].details.as_deref().expect("details")).expect("json");
        assert_eq!(details["origin"], "ble");
        assert_eq!(details["error"], "timeout");

        let published = state.relay.published();
        assert_eq!(published.len(), 1, "a lowercase code is still a code");
    }

    /// The phone tapped twice, or an agent that retries. The second invite must not queue
    /// behind the first — one pairing exchange per Hub is all the front panel ever opened.
    #[tokio::test(start_paused = true)]
    async fn a_second_invite_for_a_hub_already_pairing_is_dropped() {
        let (_app, _dir, _token, state) = hub_app(Some("hub1")).await;

        feed_invites(&state, vec![invite("hub1", None), invite("hub1", None)]).await;

        // One row, for the one exchange that ran — and by the time it is written the pair
        // timeout has elapsed, so a queued second one would have had its turn.
        let rows = wait_for_audit(&state, "device_pair", 1).await;
        assert_eq!(
            rows.len(),
            1,
            "the duplicate is dropped, not queued: {rows:?}"
        );
        assert_eq!(
            state.relay.published().len(),
            1,
            "and it never reached the device a second time"
        );
    }

    /// The System tab has one line for who paired the Hub, and `paired_by_user_id` is NULL both
    /// for a pairing nobody signed in for (GitLab #61) and for a record that went missing. The
    /// audit row is what tells them apart, so the page says which it is looking at.
    #[tokio::test]
    async fn a_pairing_with_no_user_is_rendered_as_the_devices_own_invitation() {
        let (app, _dir, token, state) = hub_app(Some("somewhere-else")).await;
        state
            .keys
            .store_pairing(
                "hub1",
                Some("test-ca".into()),
                "00".repeat(32),
                [7u8; 32],
                None,
                None,
            )
            .await
            .expect("store pairing");
        state
            .auth
            .audit(
                "device_pair",
                None,
                None,
                Some("hub1"),
                Some(json!({ "origin": "ble" }).to_string()),
                true,
            )
            .await;

        let html = body_of(get(&app, "/hubs/hub1/system", &token).await).await;
        assert!(
            html.contains("from the device itself"),
            "the invited pairing must name itself: {html}"
        );
        assert!(
            !html.contains("Paired by unknown"),
            "which is the line it replaces — that one reads as a lost record"
        );
    }

    /// The same NULL, with the audit row saying an operator did it from the portal: this one
    /// really has lost the user it was attributed to, and must not claim otherwise.
    #[tokio::test]
    async fn a_portal_pairing_whose_user_is_gone_is_not_relabelled_as_an_invitation() {
        let (app, _dir, token, state) = hub_app(Some("somewhere-else")).await;
        state
            .keys
            .store_pairing(
                "hub1",
                Some("test-ca".into()),
                "00".repeat(32),
                [7u8; 32],
                None,
                None,
            )
            .await
            .expect("store pairing");
        state
            .auth
            .audit(
                "device_pair",
                None,
                None,
                Some("hub1"),
                Some(json!({ "origin": "portal" }).to_string()),
                true,
            )
            .await;

        let html = body_of(get(&app, "/hubs/hub1/system", &token).await).await;
        assert!(!html.contains("from the device itself"), "{html}");
        assert!(html.contains("Paired by unknown"), "{html}");
    }

    /// UX-035/036: an unpaired Hub says so on the list, in the header and
    /// on the Unit tab; once paired, the line names who and when in the
    /// display zone.
    #[tokio::test]
    async fn the_pairing_state_shows_where_commands_are_offered() {
        let (app, _dir, token, state) = hub_app(Some("somewhere-else")).await;
        let list = body_of(get(&app, "/hubs", &token).await).await;
        assert!(list.contains("Not paired"), "{list}");
        assert!(list.contains(r#"href="/hubs/hub1/system""#), "{list}");
        let unit = body_of(get(&app, "/hubs/hub1/unit", &token).await).await;
        assert!(unit.contains("Not paired"), "{unit}");
        assert!(unit.contains(r#"data-pair-form="hub1""#), "{unit}");

        let admin = state
            .auth
            .list_users(true)
            .await
            .expect("users")
            .into_iter()
            .find(|u| u.username == "admin")
            .expect("admin");
        state
            .keys
            .store_pairing(
                "hub1",
                Some("test-ca".into()),
                "00".repeat(32),
                [7u8; 32],
                None,
                Some(admin.id),
            )
            .await
            .expect("store pairing");
        let list = body_of(get(&app, "/hubs", &token).await).await;
        assert!(!list.contains("Not paired"), "{list}");
        let system = body_of(get(&app, "/hubs/hub1/system", &token).await).await;
        let line = system
            .lines()
            .find(|l| l.contains("Paired by"))
            .expect("paired line");
        assert!(line.contains("admin") && line.contains("UTC</p>"), "{line}");
    }

    /// GitLab #109: an unpaired Hub says so on every tab, not on the Unit tab alone. An admin
    /// pairs from the banner (dodatek 2026-10-13), a manager gets the way to the pairing card,
    /// a viewer reads it without an action. The System tab carries the card itself, so its
    /// banner has neither. Gone once paired.
    #[tokio::test]
    async fn every_tab_of_an_unpaired_hub_points_at_pairing() {
        let (app, _dir, admin, state) = hub_app(Some("hub1")).await;
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
        let banner = r#"data-unpaired-hub="hub1""#;
        let form = r#"name="return_to""#;
        let link = r##"href="/hubs/hub1/system#pairing" class="btn-v1"##;

        for tab in ["overview", "sensors", "unit", "system"] {
            let html = body_of(get(&app, &format!("/hubs/hub1/{tab}"), &admin).await).await;
            assert!(html.contains(banner), "{tab}: {html}");
            assert_eq!(html.contains(form), tab != "system", "{tab}: {html}");
            assert!(!html.contains(link), "{tab}: {html}");
            let html = body_of(get(&app, &format!("/hubs/hub1/{tab}"), &manager).await).await;
            assert!(html.contains(banner), "{tab}, manager: {html}");
            assert!(!html.contains(form), "{tab}, manager: {html}");
            assert_eq!(
                html.contains(link),
                tab != "system",
                "{tab}, manager: {html}"
            );
            let html = body_of(get(&app, &format!("/hubs/hub1/{tab}"), &viewer).await).await;
            assert!(html.contains(banner), "{tab}, viewer: {html}");
            assert!(!html.contains(form), "{tab}, viewer: {html}");
            assert!(!html.contains(link), "{tab}, viewer: {html}");
        }
        let system = body_of(get(&app, "/hubs/hub1/system", &admin).await).await;
        assert!(
            system.contains(r#"id="pairing""#),
            "the card the links land on"
        );

        state
            .keys
            .store_pairing(
                "hub1",
                Some("test-ca".into()),
                "00".repeat(32),
                [7u8; 32],
                None,
                None,
            )
            .await
            .expect("store pairing");
        for tab in ["overview", "sensors", "unit", "system"] {
            let html = body_of(get(&app, &format!("/hubs/hub1/{tab}"), &admin).await).await;
            assert!(!html.contains(banner), "{tab}: {html}");
        }
    }

    /// Retired with FB19 (plan/control/cluster.md §7): there is no shared client password any
    /// more, and a Server hands out a credential only through Add hub, for one named Client.
    /// Not even an admin finds the route, and the account menu no longer offers it.
    #[tokio::test]
    async fn connection_info_is_retired() {
        let (app, _dir, token, _state) = hub_app(Some("hub1")).await;
        let response = get(&app, "/connection-info", &token).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let page = body_of(get(&app, "/hubs", &token).await).await;
        assert!(!page.contains("/connection-info"), "{page}");
        assert!(!page.contains("connection-info"), "no dialog either");
    }

    #[tokio::test]
    async fn the_list_renders_a_row_per_hub() {
        let (app, _dir, token, _state) = hub_app(Some("hub1")).await;
        let html = body_of(get(&app, "/hubs", &token).await).await;
        assert!(html.contains(r#"id="hub-hub1""#), "{html}");
        assert!(
            html.replace("&#x2f;", "/").contains("/hubs/hub1/overview"),
            "{html}"
        );
    }

    /// A P2P Hub in the list: LoRaWAN off by radio mode replaces the concentrator chips and
    /// the "no radio" state, and the Northbridge's state stands in, as on the Hub card.
    #[tokio::test]
    async fn the_list_shows_the_northbridge_of_a_p2p_hub() {
        use crate::state::events::RadioDiagKind;

        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        let doc: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/northbridge_retained_sample.json"
        ))
        .unwrap();
        state.store.apply(IngestEvent::RadioDiagnostics {
            hostname: "hub1".into(),
            kind: RadioDiagKind::Northbridge,
            doc: Some(doc),
        });
        state.store.apply(IngestEvent::RadioDiagnostics {
            hostname: "hub1".into(),
            kind: RadioDiagKind::Lorawan,
            doc: Some(serde_json::json!({"state": "disabled"})),
        });
        let html = body_of(get(&app, "/hubs", &token).await).await;
        for want in [r#"data-part="northbridge""#, r#"data-lorawan="off""#] {
            assert!(html.contains(want), "missing {want:?} in {html}");
        }
        for unwanted in [r#"data-part="card""#, r#"data-part="chirpstack""#] {
            assert!(!html.contains(unwanted), "{unwanted:?} in {html}");
        }
    }

    /// A dual Hub without its concentrator (a P2P Client whose RAK5146 did not enumerate):
    /// the LoRaWAN half waits, and the Northbridge keeps its own chip beside it rather than
    /// the row reading as a Hub with no radio at all.
    #[tokio::test]
    async fn the_list_keeps_the_northbridge_beside_a_missing_concentrator() {
        use crate::state::events::RadioDiagKind;

        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        state.store.apply(IngestEvent::SystemInfo {
            hostname: "hub1".to_string(),
            payload: Box::new(fiber_protocol::payloads::system_info::SystemInfoMsg {
                lorawan: Some(fiber_protocol::payloads::system_info::LorawanGatewayInfo {
                    mode: Some("dual".into()),
                    gateway_present: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        });
        let doc: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/northbridge_retained_sample.json"
        ))
        .unwrap();
        state.store.apply(IngestEvent::RadioDiagnostics {
            hostname: "hub1".into(),
            kind: RadioDiagKind::Northbridge,
            doc: Some(doc),
        });
        let html = body_of(get(&app, "/hubs", &token).await).await;
        // The card line is red (no card, #181), the Northbridge keeps its own.
        for want in [
            r#"data-part="card" data-ok="0""#,
            r#"data-part="northbridge""#,
        ] {
            assert!(html.contains(want), "missing {want:?} in {html}");
        }
    }

    /// §2.2: a Client joined through the agent's own `client-add` has no registry row; its
    /// `system/info.setup.role` still types it, in the list and on its page.
    #[tokio::test]
    async fn a_client_without_a_registry_row_is_typed_by_its_own_role() {
        use fiber_protocol::payloads::system_info::{SetupInfo, SystemInfoMsg};

        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        let list = body_of(get(&app, "/hubs", &token).await).await;
        // The unified table (devices.hub.md, amendment 2026-10-14) types
        // every Hub, a lone Server included.
        assert!(list.contains(">Server</span>"), "{list}");

        for (hostname, role) in [("hub3", None), ("proximos-2200ad33", Some("client"))] {
            state.store.apply(IngestEvent::Status {
                hostname: hostname.to_string(),
                status: DeviceStatus::Online,
            });
            state.store.apply(IngestEvent::SystemInfo {
                hostname: hostname.to_string(),
                payload: Box::new(SystemInfoMsg {
                    setup: Some(SetupInfo {
                        provisioned: Some(true),
                        role: role.map(String::from),
                    }),
                    ..Default::default()
                }),
            });
        }
        let list = body_of(get(&app, "/hubs", &token).await).await;
        for want in [">Server</span>", ">Client</span>"] {
            assert!(list.contains(want), "missing {want:?}");
        }
        let at = |h: &str| list.find(&format!("id=\"hub-{h}\"")).expect(h);
        assert!(at("hub1") < at("proximos-2200ad33"), "the Server first");
        assert!(
            at("proximos-2200ad33") < at("hub3"),
            "a Client before an untyped Hub"
        );

        let page = body_of(get(&app, "/hubs/proximos-2200ad33/overview", &token).await).await;
        assert!(page.contains(">Client</span>"), "its page says it too");
    }

    // ---- §3.2 Sensors tab: name/location commands, the row fragment ----

    /// §7: line name and location are manager+; a viewer gets 403 and no
    /// command leaves the portal.
    #[tokio::test]
    async fn sensor_name_and_location_are_manager_only() {
        let (app, _dir, _admin, state) = hub_app(Some("hub1")).await;
        let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
        for path in ["/hubs/hub1/sensors/0/name", "/hubs/hub1/sensors/0/location"] {
            let response = post(&app, path, &viewer, "value=Fridge").await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        }
    }

    /// The command names the Hub in the path (§8.2) and the line is 0..7;
    /// anything else is a 404, not a relay to the first device.
    #[tokio::test]
    async fn sensor_name_for_an_unknown_hub_or_line_is_404() {
        let (app, _dir, token, _state) = hub_app(Some("hub1")).await;
        assert_eq!(
            post(&app, "/hubs/nope/sensors/0/name", &token, "value=x")
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            post(&app, "/hubs/hub1/sensors/8/name", &token, "value=x")
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    /// A relayed `set_sensor_name` writes its own audit row with the value
    /// (§7 "ano"), whatever the device answers. Paired, so the relay accepts
    /// it; time paused, so the unanswered command times out at once.
    #[tokio::test(start_paused = true)]
    async fn sensor_name_writes_an_audit_row_with_the_value() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        state
            .keys
            .store_pairing(
                "hub1",
                Some("test-ca".into()),
                "00".repeat(32),
                [7u8; 32],
                None,
                None,
            )
            .await
            .expect("store pairing");

        let response = post(
            &app,
            "/hubs/hub1/sensors/3/name",
            &token,
            "value=+Fridge+A+",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let (rows, _total) = state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some("sensor_name_set".to_string()),
                ..Default::default()
            })
            .await
            .expect("audit page");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].target.as_deref(), Some("hub1/line-3"));
        let details: serde_json::Value =
            serde_json::from_str(rows[0].details.as_deref().unwrap_or("{}")).expect("json");
        assert_eq!(details["name"], "Fridge A", "trimmed on the way in");
        assert_eq!(details["line"], 3);
    }

    /// §3.4: the interval is range-checked here because the agent only
    /// clamps to `max(1)`. Out of range renders an error outcome and relays
    /// nothing (no audit row).
    #[tokio::test]
    async fn an_out_of_range_info_interval_is_refused_before_the_relay() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        for body in [
            "interval_seconds=5",
            "interval_seconds=90000",
            "interval_seconds=abc",
            "",
        ] {
            let response = post(&app, "/hubs/hub1/system/info-interval", &token, body).await;
            assert_eq!(response.status(), StatusCode::OK, "{body}");
            let html = body_of(response).await;
            assert!(
                html.contains("10") && html.contains("86400"),
                "{body}: {html}"
            );
        }
        let (rows, _total) = state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some("system_info_interval_set".to_string()),
                ..Default::default()
            })
            .await
            .expect("audit page");
        assert!(rows.is_empty(), "{rows:?}");
    }

    /// The interval is admin-only (§3.4, §7).
    #[tokio::test]
    async fn the_info_interval_is_admin_only() {
        let (app, _dir, _admin, state) = hub_app(Some("hub1")).await;
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let response = post(
            &app,
            "/hubs/hub1/system/info-interval",
            &manager,
            "interval_seconds=60",
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    // ---- "Změnit rádio Hubu" (§3.4, UI-24 as amended; node_registry.md §11) --

    /// `hub_app` with this Portal on `hub1`, a seam in `mode`, and `hub1` paired so the
    /// relay accepts a command (and, with time paused, times it out at once).
    async fn radio_mode_app(
        mode: control_radio::Mode,
    ) -> (
        axum::Router,
        tempfile::TempDir,
        tempfile::TempDir,
        String,
        crate::http::AppState,
    ) {
        let (_, dir, token, mut state) = hub_app(Some("hub1")).await;
        let radio_dir = tempfile::tempdir().expect("tempdir");
        state.radio = crate::radio::RadioControl::for_tests(mode, radio_dir.path());
        state
            .keys
            .store_pairing(
                "hub1",
                Some("test-ca".into()),
                "00".repeat(32),
                [7u8; 32],
                None,
                None,
            )
            .await
            .expect("store pairing");
        let router = super::router().with_state(state.clone());
        (router, dir, radio_dir, token, state)
    }

    /// `tr(key)` as the templates' HTML escaping renders it: the English strings carry
    /// apostrophes ("the Hub's radio").
    fn tr_html(key: &str) -> String {
        tr(key).replace('\'', "&#x27;")
    }

    async fn radio_mode_audit_rows(
        state: &crate::http::AppState,
    ) -> Vec<portal_storage::AuditEntry> {
        state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some("hub_radio_mode_set".to_string()),
                ..Default::default()
            })
            .await
            .expect("audit page")
            .0
    }

    /// A valid change is relayed as the signed `radio-set-mode` and audited with the mode it
    /// came from and the one it asked for.
    #[tokio::test(start_paused = true)]
    async fn a_radio_mode_change_is_relayed_and_audited_from_and_to() {
        let (app, _dir, _radio, token, state) = radio_mode_app(control_radio::Mode::P2p).await;
        let response = post(
            &app,
            "/hubs/hub1/system/radio-mode",
            &token,
            "mode=lorawan&confirm_hostname=hub1&reason=RAK+fitted",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let rows = radio_mode_audit_rows(&state).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].target.as_deref(), Some("hub1"));
        let details: serde_json::Value =
            serde_json::from_str(rows[0].details.as_deref().unwrap_or("{}")).expect("json");
        assert_eq!(details["command"], "radio-set-mode");
        assert_eq!(details["from"], "p2p");
        assert_eq!(details["to"], "lorawan");
    }

    /// `dual` is service-only (SSH): the UI does not offer it and the route refuses it.
    #[tokio::test(start_paused = true)]
    async fn dual_is_not_settable_from_the_ui() {
        let (app, _dir, _radio, token, state) = radio_mode_app(control_radio::Mode::P2p).await;
        let html = body_of(get(&app, "/hubs/hub1/system", &token).await).await;
        assert!(
            !html.contains(r#"<option value="dual""#),
            "dual is not offered"
        );
        post(
            &app,
            "/hubs/hub1/system/radio-mode",
            &token,
            "mode=dual&confirm_hostname=hub1&reason=service",
        )
        .await;
        assert!(
            radio_mode_audit_rows(&state).await.is_empty(),
            "nothing relayed"
        );
    }

    /// Admin only (§7): a manager and a viewer get 403, and nothing is audited.
    #[tokio::test]
    async fn the_radio_mode_change_is_admin_only() {
        let (app, _dir, _radio, _admin, state) = radio_mode_app(control_radio::Mode::P2p).await;
        for (role, name) in [(Role::Manager, "manager1"), (Role::Viewer, "viewer1")] {
            let token = login_as_role(&state, role, name).await;
            let response = post(
                &app,
                "/hubs/hub1/system/radio-mode",
                &token,
                "mode=lorawan&confirm_hostname=hub1",
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{role:?}");
        }
        assert!(radio_mode_audit_rows(&state).await.is_empty());
    }

    /// The typed confirm and the mode are checked before anything is signed: a wrong hostname,
    /// a mode that is not one of the four, the mode the Hub already runs and an overlong reason
    /// each render their reason and relay nothing.
    #[tokio::test(start_paused = true)]
    async fn a_radio_mode_change_needs_the_typed_hostname_and_a_new_mode() {
        let (app, _dir, _radio, token, state) = radio_mode_app(control_radio::Mode::P2p).await;
        let long_reason: &'static str = Box::leak(
            format!(
                "mode=lorawan&confirm_hostname=hub1&reason={}",
                "x".repeat(257)
            )
            .into_boxed_str(),
        );
        for (body, key) in [
            (
                "mode=lorawan&confirm_hostname=hub2",
                "monitoring.device.factory_reset_hostname_mismatch",
            ),
            (
                "mode=lorawan&confirm_hostname=",
                "monitoring.device.factory_reset_hostname_mismatch",
            ),
            (
                "mode=both&confirm_hostname=hub1",
                "monitoring.hubs.radio_change_invalid_mode",
            ),
            (
                "mode=p2p&confirm_hostname=hub1",
                "monitoring.hubs.radio_change_unchanged",
            ),
            (long_reason, "monitoring.hubs.radio_change_reason_too_long"),
        ] {
            let html =
                body_of(post(&app, "/hubs/hub1/system/radio-mode", &token, body).await).await;
            assert!(html.contains(&tr_html(key)), "{body}: {html}");
        }
        assert!(
            radio_mode_audit_rows(&state).await.is_empty(),
            "nothing was relayed"
        );
    }

    /// Only the Hub this Portal runs on: a client Hub takes the cluster's radio from the Server.
    #[tokio::test(start_paused = true)]
    async fn a_radio_mode_change_is_refused_for_another_hub() {
        let (_, dir, token, mut state) = hub_app(Some("server-hub")).await;
        let radio_dir = tempfile::tempdir().expect("tempdir");
        state.radio =
            crate::radio::RadioControl::for_tests(control_radio::Mode::P2p, radio_dir.path());
        let app = super::router().with_state(state.clone());
        let html = body_of(
            post(
                &app,
                "/hubs/hub1/system/radio-mode",
                &token,
                "mode=lorawan&confirm_hostname=hub1",
            )
            .await,
        )
        .await;
        assert!(
            html.contains(&tr_html("monitoring.hubs.radio_change_server_only")),
            "{html}"
        );
        assert!(radio_mode_audit_rows(&state).await.is_empty());
        drop(dir);
    }

    /// The System tab: the admin sees "Změnit rádio Hubu" with the current mode, and the
    /// factory-reset transport framing is gone. A manager sees neither the card's action nor
    /// the dialog.
    #[tokio::test]
    async fn the_system_tab_offers_the_radio_change_and_no_transport_reset() {
        let (app, _dir, _radio, token, state) = radio_mode_app(control_radio::Mode::P2p).await;
        let html = body_of(get(&app, "/hubs/hub1/system", &token).await).await;
        assert!(
            html.contains(&tr_html("monitoring.hubs.radio_change_heading")),
            "{html}"
        );
        assert!(html.contains("/hubs/hub1/system/radio-mode"), "{html}");
        assert!(
            html.contains(&tr_html("monitoring.hubs.radio_mode_p2p")),
            "{html}"
        );
        assert!(
            html.contains(r#"value="p2p" required x-model="mode" disabled"#),
            "the current mode is not offered: {html}"
        );
        assert!(!html.contains("switch_transport"), "{html}");

        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let html = body_of(get(&app, "/hubs/hub1/system", &manager).await).await;
        assert!(!html.contains("/hubs/hub1/system/radio-mode"), "{html}");
    }

    /// ui_components.md §13 (2026-10-15): Change radio and the factory reset sit in one
    /// locked "System management" card, an admin's only, radio first, reset last.
    #[tokio::test]
    async fn the_system_tab_groups_radio_and_reset_in_system_management() {
        let (app, _dir, _radio, token, state) = radio_mode_app(control_radio::Mode::P2p).await;
        let html = body_of(get(&app, "/hubs/hub1/system", &token).await).await;
        let card = &html[html.find("id=\"hub-system-mgmt-hub1\"").expect("card")..];
        assert!(card.contains("data-service-card"), "{card}");
        assert!(card.contains(&tr_html("monitoring.hubs.system_mgmt_heading")));
        let radio = card.find("data-svc-row=\"radio-mode\"").expect("radio row");
        let reset = card
            .find("data-svc-row=\"factory-reset\"")
            .expect("reset row");
        assert!(radio < reset, "radio first");
        assert!(card.contains("/hubs/hub1/system/factory-reset"));
        assert!(!html.contains("hub-danger-"), "{html}");

        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let html = body_of(get(&app, "/hubs/hub1/system", &manager).await).await;
        assert!(!html.contains("hub-system-mgmt-"), "{html}");
    }

    /// #470 H4: the System tab shows the P2P modulation read-only, `LoRa` from the rendered
    /// default, wherever the Hub runs P2P; a LoRaWAN-only Hub has no such row.
    #[tokio::test]
    async fn the_system_tab_shows_the_p2p_modulation_read_only() {
        for (mode, shown) in [
            (control_radio::Mode::P2p, true),
            (control_radio::Mode::Dual, true),
            (control_radio::Mode::Lorawan, false),
            (control_radio::Mode::Off, false),
        ] {
            let (app, _dir, _radio, token, _state) = radio_mode_app(mode).await;
            let html = body_of(get(&app, "/hubs/hub1/system", &token).await).await;
            assert_eq!(
                html.contains("data-p2p-modulation"),
                shown,
                "{mode:?}: {html}"
            );
            if shown {
                assert!(
                    html.contains(&tr_html("monitoring.hubs.p2p_modulation_label")),
                    "{html}"
                );
                assert!(
                    html.contains(&tr_html("monitoring.hubs.p2p_modulation_readonly")),
                    "{html}"
                );
                let row = &html[html.find("data-p2p-modulation").expect("row")..];
                assert!(
                    row[..row.find("</div>").expect("end")].contains(">LoRa"),
                    "{row}"
                );
                // Read-only: nothing in the row posts.
                assert!(
                    !row[..row.find("</div>").expect("end")].contains("hx-post"),
                    "{row}"
                );
            }
        }
    }

    /// The factory reset lost its transport framing: its audit row carries no intent.
    #[tokio::test(start_paused = true)]
    async fn a_factory_reset_no_longer_carries_a_transport_intent() {
        let (app, _dir, _radio, token, state) = radio_mode_app(control_radio::Mode::P2p).await;
        let body = "reason=decommission+unit&confirm_hostname=hub1&post_action=reboot&intent=switch_transport";
        let response = post(&app, "/hubs/hub1/system/factory-reset", &token, body).await;
        assert_eq!(response.status(), StatusCode::OK);
        let (rows, _total) = state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some("device_factory_reset".to_string()),
                ..Default::default()
            })
            .await
            .expect("audit page");
        assert_eq!(rows.len(), 1, "{rows:?}");
        let details: serde_json::Value =
            serde_json::from_str(rows[0].details.as_deref().unwrap_or("{}")).expect("json");
        assert!(details.get("intent").is_none(), "{details}");
    }

    // ---- "Přesunout všechny Nody na <rádio>" (node_registry.md §11, R6) ----------

    const LRW_NODES: [&str; 2] = ["5876070000000413", "5876070000000414"];
    const P2P_NODE: &str = "5876070000005722";

    /// Two LoRaWAN Nodes and one P2P Node in the registry of the seam under `radio_dir`.
    fn register_mixed_nodes(radio_dir: &std::path::Path) {
        for dev in LRW_NODES {
            crate::radio::test_support::register(radio_dir, portal_domain::Transport::Lorawan, dev);
        }
        crate::radio::test_support::register(radio_dir, portal_domain::Transport::P2p, P2P_NODE);
    }

    async fn node_move_audit_rows(
        state: &crate::http::AppState,
    ) -> Vec<portal_storage::AuditEntry> {
        state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some("node_move".to_string()),
                limit: 200,
                ..Default::default()
            })
            .await
            .expect("audit page")
            .0
    }

    /// Plays the Hub's part of `commands` signed commands in a row, each answered with success,
    /// while `act` runs: the relay's own request → challenge → confirm → response sequence.
    async fn with_signed_successes<F, Fut>(
        state: &crate::http::AppState,
        commands: usize,
        act: F,
    ) -> axum::response::Response
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = axum::response::Response> + Send,
    {
        let (relay_tx, relay_rx) = tokio::sync::mpsc::unbounded_channel();
        let run_relay = state.relay.clone();
        tokio::spawn(async move { run_relay.run(relay_rx).await });
        let task = tokio::spawn(async move { act().await });
        let nth = |suffix: &'static str, n: usize| async move {
            for _ in 0..600 {
                if let Some((_, body)) = state
                    .relay
                    .published()
                    .into_iter()
                    .filter(|(topic, _)| topic.ends_with(suffix))
                    .nth(n)
                {
                    return body;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("no {suffix} #{n}");
        };
        for n in 0..commands {
            let request_id = nth("commands/config/request", n).await["request_id"]
                .as_str()
                .expect("request_id")
                .to_string();
            let challenge_id = format!("chal-{}", n + 1);
            relay_tx
                .send(IngestEvent::ConfigChallenge {
                    hostname: "hub1".into(),
                    payload: fiber_protocol::payloads::config_challenge::ConfigChallengeMsg {
                        challenge_id: challenge_id.clone(),
                        request_id: Some(request_id),
                        signer_id: None,
                        expires_at: None,
                        preview: None,
                    },
                })
                .expect("challenge");
            nth("commands/config/confirm", n).await;
            relay_tx
                .send(IngestEvent::ConfigResponse {
                    hostname: "hub1".into(),
                    payload: fiber_protocol::payloads::config_response::ConfigResponseMsg {
                        challenge_id,
                        request_id: None,
                        status: "SUCCESS".into(),
                        applied_at: None,
                        effective_at: None,
                        message: Some("moved".into()),
                        result: None,
                    },
                })
                .expect("response");
        }
        task.await.expect("handler")
    }

    /// The card offers the bulk move once the Hub runs one carrier and the registry keeps Nodes
    /// on the other: the count, and the carrier it moves them to. Not on `dual`, not with no
    /// Node off.
    #[tokio::test]
    async fn the_card_offers_moving_the_nodes_off_the_switched_off_carrier() {
        let button = |to: &str| {
            tr_html("monitoring.hubs.move_nodes_button")
                .replace("{to}", &tr(&format!("monitoring.transport.{to}")))
        };
        let (app, _dir, radio_dir, token, _state) = radio_mode_app(control_radio::Mode::P2p).await;
        let html = body_of(get(&app, "/hubs/hub1/system", &token).await).await;
        assert!(!html.contains("move-nodes-dialog"), "no Node is off yet");

        register_mixed_nodes(radio_dir.path());
        let html = body_of(get(&app, "/hubs/hub1/system", &token).await).await;
        assert!(html.contains(&button("p2p")), "{html}");
        assert!(html.contains("/hubs/hub1/radio/move-nodes"), "{html}");
        assert!(
            html.contains(
                &tr_html("monitoring.hubs.move_nodes_summary")
                    .replace("{from}", &tr("monitoring.transport.lorawan"))
                    .replace("{count}", "2")
            ),
            "{html}"
        );
        assert!(html.contains(r#"name="to" value="p2p""#), "{html}");

        let (app, _dir, radio_dir, token, _state) = radio_mode_app(control_radio::Mode::Dual).await;
        register_mixed_nodes(radio_dir.path());
        let html = body_of(get(&app, "/hubs/hub1/system", &token).await).await;
        assert!(!html.contains("move-nodes-dialog"), "dual runs both");
    }

    /// One signed `node-move` per Node on the switched-off carrier, to the carrier the Hub runs,
    /// and one audit row `node_move` per Node (`from`, `to`, `bulk`). The Node already on the
    /// running carrier is not touched, and the result reminds the operator to switch the Nodes.
    #[tokio::test]
    async fn moving_all_nodes_sends_one_signed_move_and_one_audit_row_per_node() {
        let (app, _dir, radio_dir, token, state) = radio_mode_app(control_radio::Mode::P2p).await;
        register_mixed_nodes(radio_dir.path());
        let response = with_signed_successes(&state, LRW_NODES.len(), move || async move {
            post(
                &app,
                "/hubs/hub1/radio/move-nodes",
                &token,
                "to=p2p&confirm_hostname=hub1",
            )
            .await
        })
        .await;
        let html = body_of(response).await;
        let result = tr_html("monitoring.hubs.move_nodes_result")
            .replace("{to}", &tr("monitoring.transport.p2p"))
            .replace("{moved}", "2")
            .replace("{total}", "2");
        assert!(html.contains(&result), "{html}");

        let requests: Vec<serde_json::Value> = state
            .relay
            .published()
            .into_iter()
            .filter(|(topic, _)| topic.ends_with("commands/config/request"))
            .map(|(_, body)| body)
            .collect();
        assert_eq!(requests.len(), LRW_NODES.len());
        for (body, dev) in requests.iter().zip(LRW_NODES) {
            assert_eq!(body["command_type"], "node-move");
            assert_eq!(body["level"], "admin");
            assert_eq!(body["params"], json!({ "dev_eui": dev, "to": "p2p" }));
        }

        let mut rows = node_move_audit_rows(&state).await;
        rows.sort_by(|a, b| a.target.cmp(&b.target));
        assert_eq!(rows.len(), LRW_NODES.len(), "{rows:?}");
        for (row, dev) in rows.iter().zip(LRW_NODES) {
            assert_eq!(row.target.as_deref(), Some(dev));
            assert!(row.success, "{row:?}");
            let details: serde_json::Value =
                serde_json::from_str(row.details.as_deref().unwrap_or("{}")).expect("json");
            assert_eq!(details["from"], "lorawan");
            assert_eq!(details["to"], "p2p");
            assert_eq!(details["bulk"], true);
        }
    }

    /// A Node the Hub does not confirm (the test relay's silence) is reported and audited as
    /// failed, and does not stop the others.
    #[tokio::test(start_paused = true)]
    async fn a_node_the_hub_does_not_move_is_reported_and_the_rest_still_go() {
        let (app, _dir, radio_dir, token, state) = radio_mode_app(control_radio::Mode::P2p).await;
        register_mixed_nodes(radio_dir.path());
        let html = body_of(
            post(
                &app,
                "/hubs/hub1/radio/move-nodes",
                &token,
                "to=p2p&confirm_hostname=hub1",
            )
            .await,
        )
        .await;
        assert!(html.contains("alert-error"), "{html}");
        for dev in LRW_NODES {
            assert!(html.contains(dev), "{dev} named: {html}");
        }
        let rows = node_move_audit_rows(&state).await;
        assert_eq!(rows.len(), LRW_NODES.len(), "every Node was tried");
        assert!(rows.iter().all(|row| !row.success), "{rows:?}");
    }

    /// Admin only, the typed hostname, a Node off to move and the carrier the card offered —
    /// each checked before anything is signed.
    #[tokio::test]
    async fn moving_all_nodes_is_refused_before_signing_without_its_preconditions() {
        let (app, _dir, radio_dir, token, state) = radio_mode_app(control_radio::Mode::P2p).await;
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let html = body_of(
            post(
                &app,
                "/hubs/hub1/radio/move-nodes",
                &token,
                "to=p2p&confirm_hostname=hub1",
            )
            .await,
        )
        .await;
        assert!(
            html.contains(&tr_html("monitoring.hubs.move_nodes_none")),
            "{html}"
        );

        register_mixed_nodes(radio_dir.path());
        assert_eq!(
            post(
                &app,
                "/hubs/hub1/radio/move-nodes",
                &manager,
                "to=p2p&confirm_hostname=hub1",
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
        for (body, key) in [
            (
                "to=p2p&confirm_hostname=hub2",
                "monitoring.device.factory_reset_hostname_mismatch",
            ),
            (
                "to=lorawan&confirm_hostname=hub1",
                "monitoring.hubs.move_nodes_stale",
            ),
        ] {
            let html = body_of(post(&app, "/hubs/hub1/radio/move-nodes", &token, body).await).await;
            assert!(html.contains(&tr_html(key)), "{body}: {html}");
        }
        assert!(state.relay.published().is_empty(), "nothing was signed");
        assert!(node_move_audit_rows(&state).await.is_empty());
    }

    // ---- "Změnit rádio Hubu" asks about the Nodes (node_registry.md §12, R8) ------

    /// The signed requests the relay published, in order: `(command_type, params)`.
    fn published_requests(state: &crate::http::AppState) -> Vec<(String, serde_json::Value)> {
        state
            .relay
            .published()
            .into_iter()
            .filter(|(topic, _)| topic.ends_with("commands/config/request"))
            .map(|(_, body)| {
                (
                    body["command_type"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    body["params"].clone(),
                )
            })
            .collect()
    }

    async fn audit_rows_of(
        state: &crate::http::AppState,
        event: &str,
    ) -> Vec<portal_storage::AuditEntry> {
        state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some(event.to_string()),
                limit: 200,
                ..Default::default()
            })
            .await
            .expect("audit page")
            .0
    }

    fn details_of(row: &portal_storage::AuditEntry) -> serde_json::Value {
        serde_json::from_str(row.details.as_deref().unwrap_or("{}")).expect("json")
    }

    /// The dialog asks, per mode, only where the mode switches off a carrier with Nodes on it,
    /// and offers the move only where the new mode runs a carrier. On a LoRaWAN Hub with two
    /// LoRaWAN Nodes: `p2p` and `off` ask, `dual` does not; `off` has no move.
    #[tokio::test]
    async fn the_dialog_asks_about_the_nodes_only_where_a_carrier_with_nodes_goes_off() {
        let (app, _dir, radio_dir, token, _state) =
            radio_mode_app(control_radio::Mode::Lorawan).await;
        let html = body_of(get(&app, "/hubs/hub1/system", &token).await).await;
        assert!(
            !html.contains("data-nodes-for"),
            "no Node yet, no question: {html}"
        );

        register_mixed_nodes(radio_dir.path());
        let html = body_of(get(&app, "/hubs/hub1/system", &token).await).await;
        let fieldset = |mode: &str| -> Option<String> {
            let at = html.find(&format!(r#"data-nodes-for="{mode}""#))?;
            let end = html[at..].find("</fieldset>").expect("closed") + at;
            Some(html[at..end].to_string())
        };
        let to_p2p = fieldset("p2p").expect("p2p switches LoRaWAN off");
        assert!(to_p2p.contains(r#"value="move""#), "{to_p2p}");
        assert!(to_p2p.contains(r#"value="remove""#), "{to_p2p}");
        assert!(to_p2p.contains(r#"value="keep""#), "{to_p2p}");
        assert!(
            to_p2p.contains(
                &tr_html("monitoring.hubs.radio_change_nodes_move")
                    .replace("{to}", &tr("monitoring.transport.p2p"))
            ),
            "{to_p2p}"
        );
        assert!(
            to_p2p.contains(
                &tr_html("monitoring.hubs.radio_change_nodes_legend")
                    .replace("{from}", &tr("monitoring.transport.lorawan"))
                    .replace("{count}", "2")
            ),
            "{to_p2p}"
        );
        let to_off = fieldset("off").expect("off switches LoRaWAN off");
        assert!(!to_off.contains(r#"value="move""#), "{to_off}");
        assert!(
            to_off.contains(&tr_html("monitoring.hubs.radio_change_nodes_move_off")),
            "{to_off}"
        );
        assert!(fieldset("dual").is_none(), "dual switches nothing off");
    }

    /// A switch that leaves Nodes on a carrier it switches off needs the choice; a missing or
    /// unknown one, and a move where the new mode runs no carrier, are refused before anything
    /// is signed.
    #[tokio::test(start_paused = true)]
    async fn a_switch_with_nodes_on_the_carrier_it_switches_off_needs_a_valid_choice() {
        let (app, _dir, radio_dir, token, state) =
            radio_mode_app(control_radio::Mode::Lorawan).await;
        register_mixed_nodes(radio_dir.path());
        for (body, key) in [
            (
                "mode=p2p&confirm_hostname=hub1",
                "monitoring.hubs.radio_change_nodes_required",
            ),
            (
                "mode=p2p&confirm_hostname=hub1&nodes=delete",
                "monitoring.hubs.radio_change_nodes_required",
            ),
            (
                "mode=off&confirm_hostname=hub1&nodes=move",
                "monitoring.hubs.radio_change_nodes_move_unavailable",
            ),
        ] {
            let html =
                body_of(post(&app, "/hubs/hub1/system/radio-mode", &token, body).await).await;
            assert!(html.contains(&tr_html(key)), "{body}: {html}");
        }
        assert!(state.relay.published().is_empty(), "nothing was signed");
        assert!(radio_mode_audit_rows(&state).await.is_empty());
    }

    /// „převést na nové rádio“: one signed `node-move` per Node, then `radio-set-mode` — in
    /// that order, so the old carrier is still on for the moves. One audit row per Node, and
    /// the switch's row names the choice.
    #[tokio::test]
    async fn moving_the_nodes_runs_before_the_switch() {
        let (app, _dir, radio_dir, token, state) =
            radio_mode_app(control_radio::Mode::Lorawan).await;
        register_mixed_nodes(radio_dir.path());
        let response = with_signed_successes(&state, LRW_NODES.len() + 1, move || async move {
            post(
                &app,
                "/hubs/hub1/system/radio-mode",
                &token,
                "mode=p2p&confirm_hostname=hub1&nodes=move",
            )
            .await
        })
        .await;
        let html = body_of(response).await;
        assert!(
            html.contains(
                &tr_html("monitoring.hubs.move_nodes_result")
                    .replace("{to}", &tr("monitoring.transport.p2p"))
                    .replace("{moved}", "2")
                    .replace("{total}", "2")
            ),
            "{html}"
        );

        let requests = published_requests(&state);
        assert_eq!(
            requests,
            vec![
                (
                    "node-move".to_string(),
                    json!({ "dev_eui": LRW_NODES[0], "to": "p2p" })
                ),
                (
                    "node-move".to_string(),
                    json!({ "dev_eui": LRW_NODES[1], "to": "p2p" })
                ),
                ("radio-set-mode".to_string(), json!({ "mode": "p2p" })),
            ],
            "the Node step first, then the switch"
        );
        let moves = node_move_audit_rows(&state).await;
        assert_eq!(moves.len(), 2, "{moves:?}");
        assert!(moves
            .iter()
            .all(|r| r.success && details_of(r)["bulk"] == true));
        let switch = radio_mode_audit_rows(&state).await;
        assert_eq!(switch.len(), 1);
        assert_eq!(details_of(&switch[0])["nodes"], "move");
        assert_eq!(details_of(&switch[0])["nodes_failed"], json!([]));
    }

    /// „smazat“: one signed `node-remove` per Node on its own carrier, then the switch.
    #[tokio::test]
    async fn removing_the_nodes_runs_before_the_switch() {
        let (app, _dir, radio_dir, token, state) =
            radio_mode_app(control_radio::Mode::Lorawan).await;
        register_mixed_nodes(radio_dir.path());
        let response = with_signed_successes(&state, LRW_NODES.len() + 1, move || async move {
            post(
                &app,
                "/hubs/hub1/system/radio-mode",
                &token,
                "mode=p2p&confirm_hostname=hub1&nodes=remove",
            )
            .await
        })
        .await;
        let html = body_of(response).await;
        assert!(
            html.contains(
                &tr_html("monitoring.hubs.radio_change_nodes_removed")
                    .replace("{done}", "2")
                    .replace("{total}", "2")
            ),
            "{html}"
        );
        assert_eq!(
            published_requests(&state),
            vec![
                (
                    "node-remove".to_string(),
                    json!({ "radio": "lorawan", "dev_eui": LRW_NODES[0] })
                ),
                (
                    "node-remove".to_string(),
                    json!({ "radio": "lorawan", "dev_eui": LRW_NODES[1] })
                ),
                ("radio-set-mode".to_string(), json!({ "mode": "p2p" })),
            ]
        );
        let removals = audit_rows_of(&state, "node_remove").await;
        assert_eq!(removals.len(), 2, "{removals:?}");
        for row in &removals {
            assert!(row.success, "{row:?}");
            assert_eq!(details_of(row)["transport"], "lorawan");
            assert_eq!(details_of(row)["bulk"], true);
        }
        assert!(node_move_audit_rows(&state).await.is_empty());
        assert_eq!(
            details_of(&radio_mode_audit_rows(&state).await[0])["nodes"],
            "remove"
        );
    }

    /// „nechat na starém protokolu“: only the switch is sent; the result says the Nodes stay.
    #[tokio::test]
    async fn keeping_the_nodes_sends_only_the_switch() {
        let (app, _dir, radio_dir, token, state) =
            radio_mode_app(control_radio::Mode::Lorawan).await;
        register_mixed_nodes(radio_dir.path());
        let response = with_signed_successes(&state, 1, move || async move {
            post(
                &app,
                "/hubs/hub1/system/radio-mode",
                &token,
                "mode=p2p&confirm_hostname=hub1&nodes=keep",
            )
            .await
        })
        .await;
        let html = body_of(response).await;
        assert!(
            html.contains(
                &tr_html("monitoring.hubs.radio_change_nodes_kept").replace("{count}", "2")
            ),
            "{html}"
        );
        assert_eq!(
            published_requests(&state),
            vec![("radio-set-mode".to_string(), json!({ "mode": "p2p" }))]
        );
        assert_eq!(
            details_of(&radio_mode_audit_rows(&state).await[0])["nodes"],
            "keep"
        );
    }

    /// A Node the Hub does not confirm does not stop the others or the switch: every Node is
    /// tried, the switch is still sent, and the result and the audit row name the failures.
    #[tokio::test(start_paused = true)]
    async fn a_failing_node_stops_neither_the_others_nor_the_switch() {
        let (app, _dir, radio_dir, token, state) =
            radio_mode_app(control_radio::Mode::Lorawan).await;
        register_mixed_nodes(radio_dir.path());
        let html = body_of(
            post(
                &app,
                "/hubs/hub1/system/radio-mode",
                &token,
                "mode=p2p&confirm_hostname=hub1&nodes=move",
            )
            .await,
        )
        .await;
        let kinds: Vec<String> = published_requests(&state)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(kinds, ["node-move", "node-move", "radio-set-mode"]);
        for dev in LRW_NODES {
            assert!(html.contains(dev), "{dev} named: {html}");
        }
        let moves = node_move_audit_rows(&state).await;
        assert_eq!(moves.len(), 2);
        assert!(moves.iter().all(|r| !r.success));
        let switch = radio_mode_audit_rows(&state).await;
        assert_eq!(switch.len(), 1, "the switch was sent and audited");
        assert_eq!(
            details_of(&switch[0])["nodes_failed"],
            json!([LRW_NODES[0], LRW_NODES[1]])
        );
        assert!(
            html.contains(&tr_html("monitoring.hubs.radio_change_nodes_switch_failed")),
            "the switch timed out too, and the result says the Node step already ran: {html}"
        );
    }

    /// Admin only with the Node choice too: a manager's switch-with-move is a 403, and no Node
    /// is touched.
    #[tokio::test]
    async fn the_node_step_is_admin_only() {
        let (app, _dir, radio_dir, _admin, state) =
            radio_mode_app(control_radio::Mode::Lorawan).await;
        register_mixed_nodes(radio_dir.path());
        for (role, name) in [(Role::Manager, "manager1"), (Role::Viewer, "viewer1")] {
            let token = login_as_role(&state, role, name).await;
            let response = post(
                &app,
                "/hubs/hub1/system/radio-mode",
                &token,
                "mode=p2p&confirm_hostname=hub1&nodes=remove",
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{role:?}");
        }
        assert!(state.relay.published().is_empty());
        assert!(audit_rows_of(&state, "node_remove").await.is_empty());
    }

    /// The Sensors tab renders one self-refreshing row per line with the
    /// six threshold cells, and the row fragment route serves the same row.
    #[tokio::test]
    async fn the_sensors_tab_and_the_row_fragment_render_the_line() {
        let (app, _dir, token, _state) = hub_app(Some("hub1")).await;
        let html = body_of(get(&app, "/hubs/hub1/sensors", &token).await).await;
        assert!(html.contains(r#"id="hub-sensor-hub1-0""#), "{html}");
        assert!(html.contains(r#"hx-trigger="sse:sensor-hub1-0""#), "{html}");
        assert!(
            html.replace("&#x2f;", "/")
                .contains(r#"data-row-href="/hubs/hub1/sensors/line-1""#),
            "the row opens the probe's page: {html}"
        );
        let page = body_of(get(&app, "/hubs/hub1/sensors/line-1", &token).await).await;
        assert!(
            page.replace("&#x2f;", "/")
                .contains("/hubs/hub1/sensors/0/meta"),
            "admin edits the name on the probe page: {page}"
        );

        let row = body_of(get(&app, "/hubs/hub1/sensors/7/row", &token).await).await;
        assert!(row.contains(r#"id="hub-sensor-hub1-7""#), "{row}");
        assert_eq!(
            get(&app, "/hubs/hub1/sensors/8/row", &token).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    /// ui_components.md §2: the wired probe's page shows the Channels card
    /// (no chart) in place of the probe window's chart and keeps the events
    /// with the CSV export.
    #[tokio::test]
    async fn the_probe_page_carries_the_channels_card_and_the_events() {
        let (app, _dir, token, _state) = hub_app(Some("hub1")).await;
        let page = body_of(get(&app, "/hubs/hub1/sensors/line-2", &token).await)
            .await
            .replace("&#x2f;", "/");
        assert!(page.contains(r#"id="channels-line-1""#), "{page}");
        // ui_components.md §12: the Trends link is the Channels card's only.
        assert_eq!(page.matches("/trends?s=line:hub1:1").count(), 1, "{page}");
        assert!(page.contains(r#"id="diag-probe-1""#), "{page}");
        assert!(
            page.contains(r#"data-row-id="avg""#) && !page.contains(r#"data-row-id="min""#),
            "the probe lists its reading only: {page}"
        );
        assert!(
            page.contains("Set channels") && page.contains("/hubs/hub1/sensors/1/threshold"),
            "the probe's limits are edited from the channels card: {page}"
        );
        assert!(!page.contains("chart-island"), "the card has no chart");
        assert!(
            !page.contains(r#"id="probe-png""#),
            "the window's chart is gone"
        );
        assert!(
            page.contains("data-events-card")
                && page.contains("/monitoring/probe/probe:1/events")
                && !page.contains("probe-event-form"),
            "the Events card replaces the events-in-range list: {page}"
        );
    }

    /// UX-031: a real Hub reports an uppercase hostname (`FIBER-MOCK-001`)
    /// while the store keys devices lowercased. The row URL the tab renders
    /// carried the wire spelling, and the row handler indexed the map with
    /// it, so every SSE refetch 404'd and the tab froze. Since decision #21
    /// the hostname has one spelling, lowercase, so the row carries that.
    #[tokio::test]
    async fn the_row_fragment_serves_an_uppercase_hostname() {
        let (app, _dir, token, state) = hub_app(None).await;
        state.store.apply(IngestEvent::Status {
            hostname: "FIBER-MOCK-001".to_string(),
            status: DeviceStatus::Online,
        });
        let html = body_of(get(&app, "/hubs/FIBER-MOCK-001/sensors", &token).await).await;
        let row_url = html
            .split(r#"hx-get=""#)
            .skip(1)
            .filter_map(|rest| rest.split_once('"').map(|(url, _)| url))
            .find(|url| url.ends_with("/sensors/0/row"))
            .unwrap_or_else(|| panic!("no row url in the tab: {html}"))
            .to_string();

        let response = get(&app, &row_url, &token).await;
        assert_eq!(response.status(), StatusCode::OK, "{row_url}");
        assert!(body_of(response)
            .await
            .contains(r#"id="hub-sensor-fiber-mock-001-0""#));
    }

    // ---- Name and description (devices.hub.md §3.1, §7, §12 D) ---------

    /// The row the meta POST writes, or `None`.
    async fn stored_meta(
        state: &crate::http::AppState,
        hostname: &str,
    ) -> Option<portal_storage::DeviceMeta> {
        ({
            let a0 = hostname.to_string();
            let a1 = portal_storage::THRESHOLD_SCOPE_HUB.to_string();
            let a2 = hostname.to_string();
            state.db.run(move |s| s.get_device_meta(&a0, &a1, &a2))
        })
        .await
        .expect("read device_meta")
    }

    /// Naming a Hub is manager+ (§7), and the refusal has to leave the
    /// database untouched — a 403 that still wrote the row would be a gate
    /// in name only.
    #[tokio::test]
    async fn a_viewer_cannot_name_a_hub() {
        let (app, _dir, _admin, state) = hub_app(Some("hub1")).await;
        let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;

        let response = post(
            &app,
            "/hubs/hub1/meta",
            &viewer,
            "name=Sklad+B&description=",
        )
        .await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(stored_meta(&state, "hub1").await.is_none());
    }

    /// An unknown `{hub}` is a 404 and writes nothing: `device_meta` is
    /// keyed by hostname, so a row planted for a name this portal does not
    /// serve would be inherited by whatever Hub claims it later.
    #[tokio::test]
    async fn naming_an_unknown_hub_is_a_404_and_writes_nothing() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;

        let response = post(&app, "/hubs/nope/meta", &token, "name=Ghost&description=").await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(stored_meta(&state, "nope").await.is_none());
        assert!(stored_meta(&state, "hub1").await.is_none());
    }

    /// The write path end to end: the row, the audit entry (§7) and the
    /// redirect back to Overview with its flash slug. Then a blank save,
    /// because clearing is the other half of the contract — there is no
    /// "inherit" state for a name, so blank means no name.
    #[tokio::test]
    async fn a_manager_names_a_hub_and_the_write_is_audited() {
        let (app, _dir, _admin, state) = hub_app(Some("hub1")).await;
        let manager = login_as_role(&state, Role::Manager, "manager1").await;

        let response = post(
            &app,
            "/hubs/hub1/meta",
            &manager,
            "name=Sklad+B&description=cabinet+by+the+dock",
        )
        .await;

        assert_eq!(location(&response), "/hubs/hub1/overview?ok=meta_saved");
        let row = stored_meta(&state, "hub1")
            .await
            .expect("the row was written");
        assert_eq!(row.name.as_deref(), Some("Sklad B"));
        assert_eq!(row.description.as_deref(), Some("cabinet by the dock"));
        assert_eq!(row.scope, portal_storage::THRESHOLD_SCOPE_HUB);
        assert_eq!(row.entity, "hub1", "a Hub is its own entity");

        let (rows, _total) = state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some("hub_meta_set".to_string()),
                ..Default::default()
            })
            .await
            .expect("audit page");
        assert_eq!(rows.len(), 1, "one hub_meta_set row: {rows:?}");
        assert_eq!(rows[0].target.as_deref(), Some("hub1"));
        assert_eq!(rows[0].username.as_deref(), Some("manager1"));
        assert!(rows[0].success);

        // Blank clears both, and the header falls back on its own.
        let response = post(&app, "/hubs/hub1/meta", &manager, "name=&description=").await;
        assert_eq!(location(&response), "/hubs/hub1/overview?ok=meta_saved");
        let row = stored_meta(&state, "hub1")
            .await
            .expect("the row is still there");
        assert_eq!(row.name, None);
        assert_eq!(row.description, None);
    }

    /// §3.1: the operator's name beats the `device_label` the unit reports,
    /// on the list and in the detail header alike — that is the whole point
    /// of holding it portal-side, since a factory reset takes the label.
    #[tokio::test]
    async fn the_stored_name_wins_over_the_reported_device_label() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        state.store.apply(IngestEvent::SystemInfo {
            hostname: "hub1".to_string(),
            payload: Box::new(fiber_protocol::payloads::system_info::SystemInfoMsg {
                device_label: Some("FIBER from the factory".to_string()),
                ..Default::default()
            }),
        });

        // Before naming: the reported label is what both surfaces show.
        let list = body_of(get(&app, "/hubs", &token).await).await;
        assert!(list.contains("FIBER from the factory"), "{list}");

        ({
            let a0 = portal_storage::NewDeviceMeta {
                name: Some("Sklad B".into()),
                description: Some("cabinet by the dock".into()),
                ..portal_storage::NewDeviceMeta::new(
                    "hub1",
                    portal_storage::THRESHOLD_SCOPE_HUB,
                    "hub1",
                )
            };
            state.db.write(move |s| s.upsert_device_meta(&a0))
        })
        .await
        .expect("upsert device_meta");

        let list = body_of(get(&app, "/hubs", &token).await).await;
        assert!(list.contains("Sklad B"), "{list}");
        assert!(
            !list.contains("FIBER from the factory"),
            "the operator's name replaces the label, it does not sit beside it: {list}"
        );
        // And the fulltext filter can find it (§2.2 runs over `data-search`).
        assert!(list.contains(r#"data-search="sklad b hub1"#), "{list}");

        // The detail header and the tab title, which are the two places the
        // Hub is *named*. The reported label keeps its own line in the
        // System status card — that one is a fact about the unit, not a
        // second name for it, so it is not asserted away here.
        let detail = body_of(get(&app, "/hubs/hub1/overview", &token).await).await;
        assert!(
            detail.contains("<title>Sklad B - ProXimos</title>"),
            "{detail}"
        );
        assert!(detail.contains(">Sklad B</h2>"), "{detail}");
        assert!(!detail.contains(">FIBER from the factory</h2>"), "{detail}");
        assert!(detail.contains("cabinet by the dock"), "{detail}");
    }

    // ---- §3.1 Device description (amendment 2026-09-28) ----------------

    /// The Overview's Device information block alone (metadata.cs.md §5):
    /// its section through its `<dl>` of facts.
    fn description_card(html: &str) -> &str {
        let at = html
            .find(r#"data-device-info="hub""#)
            .unwrap_or_else(|| panic!("no device information block: {html}"));
        let len = html[at..].find("</dl>").expect("the card's </dl>");
        &html[at..at + len]
    }

    /// What the description card shows beside `label_key`: the text of the
    /// `<dd>` that follows its `<dt>` (the template's `fact`). minijinja's
    /// autoescape also spells "/" as an entity ("Model / revision").
    fn fact_value(html: &str, label_key: &str) -> String {
        let card = description_card(html);
        let dt = format!(">{}</dt>", tr_html(label_key).replace('/', "&#x2f;"));
        let at = card
            .find(&dt)
            .unwrap_or_else(|| panic!("no {label_key} row: {card}"));
        let rest = &card[at + dt.len()..];
        let open = rest.find("<dd").expect("a <dd> follows the <dt>");
        let start = open + rest[open..].find('>').expect("<dd> closes") + 1;
        let end = start + rest[start..].find("</dd>").expect("</dd>");
        // The value up to the copy button, if the fact has one.
        let value = &rest[start..end];
        value[..value.find('<').unwrap_or(value.len())]
            .trim()
            .to_string()
    }

    fn system_info(
        mode: &str,
        hardware: Option<fiber_protocol::payloads::system_info::Hardware>,
    ) -> IngestEvent {
        IngestEvent::SystemInfo {
            hostname: "hub1".to_string(),
            payload: Box::new(fiber_protocol::payloads::system_info::SystemInfoMsg {
                lorawan: Some(fiber_protocol::payloads::system_info::LorawanGatewayInfo {
                    mode: Some(mode.to_string()),
                    gateway_eui: Some("70B3D54900AA0001".to_string()),
                    ..Default::default()
                }),
                hardware,
                ..Default::default()
            }),
        }
    }

    /// §3.1 as amended 2026-09-28 (Hynek, after c61): the card shows the CPU
    /// serial, the compute module's model and revision and the radio mode
    /// with the System tab's label.
    #[tokio::test]
    async fn the_description_card_shows_serial_model_and_radio_mode() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        let cm4 = fiber_protocol::payloads::system_info::Hardware {
            model: Some("Raspberry Pi Compute Module 4".to_string()),
            revision: Some("1.1".to_string()),
            serial: Some("100000004e970f4a".to_string()),
        };
        state.store.apply(system_info("p2p", Some(cm4)));

        let html = body_of(get(&app, "/hubs/hub1/overview", &token).await).await;
        assert_eq!(
            fact_value(&html, "monitoring.device_info.serial"),
            "100000004e970f4a"
        );
        // The "/" autoescaped, as in the label.
        assert_eq!(
            fact_value(&html, "monitoring.device_info.model"),
            "Raspberry Pi Compute Module 4 &#x2f; 1.1"
        );
        assert_eq!(
            fact_value(&html, "monitoring.device_info.radio_mode"),
            tr_html("monitoring.hubs.radio_mode_p2p")
        );
        // And the list's fulltext finds the same serial.
        let list = body_of(get(&app, "/hubs", &token).await).await;
        assert!(list.contains("hub1 100000004e970f4a"), "{list}");
    }

    /// Hynek, 2026-09-28: the gateway EUI is not a row of the card in any
    /// mode — it is shown once, under the radio card's gateways. The card is
    /// serial, model / revision, radio mode, agent version and uptime, plus
    /// the hostname since the Device information block (metadata.cs.md §5).
    #[tokio::test]
    async fn the_description_card_has_no_gateway_eui_row() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        for mode in ["off", "lorawan", "p2p", "dual"] {
            state.store.apply(system_info(mode, None));
            let html = body_of(get(&app, "/hubs/hub1/overview", &token).await).await;
            let card = description_card(&html);
            assert_eq!(card.matches("<dt").count(), 6, "{mode}: {card}");
            assert!(!card.contains("70b3d54900aa0001"), "{mode}: {card}");
            assert!(!card.contains("Gateway EUI"), "{mode}: {card}");
        }
    }

    /// A `system/info` from an agent that predates `hardware` and `mode`:
    /// the first three facts are "—", nothing is invented.
    #[tokio::test]
    async fn an_old_system_info_leaves_the_description_unknown() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        state.store.apply(IngestEvent::SystemInfo {
            hostname: "hub1".to_string(),
            payload: Box::new(fiber_protocol::payloads::system_info::SystemInfoMsg {
                firmware_version: Some("0.1.0".to_string()),
                ..Default::default()
            }),
        });

        let html = body_of(get(&app, "/hubs/hub1/overview", &token).await).await;
        for key in [
            "monitoring.device_info.serial",
            "monitoring.device_info.model",
            "monitoring.device_info.radio_mode",
        ] {
            assert_eq!(fact_value(&html, key), "—", "{key}");
        }
    }

    // ---- §3.3 Unit tab: display, buzzer, label, the LCD picker ----

    /// display-config.md §3: the agent casts the scalars without clamping,
    /// so the portal is the range check. Out of range renders an error
    /// outcome and relays nothing — no audit row behind it.
    #[tokio::test]
    async fn an_out_of_range_display_value_is_refused_before_the_relay() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        for (path, body) in [
            ("/hubs/hub1/display/buzzer-volume", "value=101"),
            ("/hubs/hub1/display/screen-brightness", "value=300"),
            ("/hubs/hub1/display/led-brightness", "value=-1"),
            ("/hubs/hub1/display/screen-timeout", "value=86401"),
            ("/hubs/hub1/display/buzzer-volume", "value=abc"),
            ("/hubs/hub1/display/buzzer-volume", ""),
        ] {
            let response = post(&app, path, &token, body).await;
            assert_eq!(response.status(), StatusCode::OK, "{path} {body}");
        }
        // An empty label is refused the same way.
        assert_eq!(
            post(&app, "/hubs/hub1/display/device-label", &token, "value=+++")
                .await
                .status(),
            StatusCode::OK
        );

        for event in [
            "display_buzzer_volume_set",
            "display_screen_brightness_set",
            "display_led_brightness_set",
            "display_screen_timeout_set",
            "display_device_label_set",
        ] {
            let (rows, _total) = state
                .auth
                .audit_page(portal_storage::AuditFilter {
                    event_type: Some(event.to_string()),
                    ..Default::default()
                })
                .await
                .expect("audit page");
            assert!(rows.is_empty(), "{event}: {rows:?}");
        }
    }

    /// Manager+ (§3.3/§7): a viewer gets 403 on every display route and no
    /// command leaves the portal.
    #[tokio::test]
    async fn the_display_routes_are_manager_only() {
        let (app, _dir, _admin, state) = hub_app(Some("hub1")).await;
        let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
        for (path, body) in [
            ("/hubs/hub1/display/buzzer-volume", "value=50"),
            ("/hubs/hub1/display/screen-brightness", "value=50"),
            ("/hubs/hub1/display/led-brightness", "value=50"),
            ("/hubs/hub1/display/screen-timeout", "value=60"),
            ("/hubs/hub1/display/device-label", "value=Ward"),
            ("/hubs/hub1/display/sensors", "sensors=ds18b20%3A0"),
        ] {
            let response = post(&app, path, &viewer, body).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        }
    }

    /// The command names the Hub in its path (§8.2 rule 2), and an unknown
    /// setting slug is an unserved path, not a relayed command.
    #[tokio::test]
    async fn an_unknown_hub_or_setting_is_404() {
        let (app, _dir, token, _state) = hub_app(Some("hub1")).await;
        for path in [
            "/hubs/nope/display/buzzer-volume",
            "/hubs/nope/display/sensors",
            "/hubs/hub1/display/contrast",
        ] {
            assert_eq!(
                post(&app, path, &token, "value=50").await.status(),
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
        assert_eq!(
            get(&app, "/hubs/nope/display/picker", &token)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    /// DC-3: over 8 is refused here, whatever the browser allowed, and
    /// nothing is relayed.
    #[tokio::test]
    async fn more_than_eight_sensors_is_refused_before_the_relay() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        // Nine ids: the eight wired lines plus a repeat.
        let body = "sensors=ds18b20%3A0&sensors=ds18b20%3A1&sensors=ds18b20%3A2\
&sensors=ds18b20%3A3&sensors=ds18b20%3A4&sensors=ds18b20%3A5&sensors=ds18b20%3A6\
&sensors=ds18b20%3A7&sensors=ds18b20%3A0";
        let response = post(&app, "/hubs/hub1/display/sensors", &token, body).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_of(response).await.contains('8'));

        let (rows, _total) = state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some("display_lines_set".to_string()),
                ..Default::default()
            })
            .await
            .expect("audit page");
        assert!(rows.is_empty(), "{rows:?}");
    }

    /// An id the catalogue cannot resolve never reaches the wire either.
    #[tokio::test]
    async fn an_unresolvable_sensor_id_is_refused() {
        let (app, _dir, token, _state) = hub_app(Some("hub1")).await;
        let response = post(
            &app,
            "/hubs/hub1/display/sensors",
            &token,
            "sensors=ds18b20%3A99",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_of(response).await.contains("ds18b20:99"));
    }

    /// A relayed pick writes its audit row with the ids it sent (§7), even
    /// though nothing answers the command. Paired so the relay accepts it,
    /// time paused so the unanswered command times out at once.
    #[tokio::test(start_paused = true)]
    async fn a_sensor_pick_audits_the_ids_it_sent() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        state
            .keys
            .store_pairing(
                "hub1",
                Some("test-ca".into()),
                "00".repeat(32),
                [7u8; 32],
                None,
                None,
            )
            .await
            .expect("store pairing");

        let response = post(
            &app,
            "/hubs/hub1/display/sensors",
            &token,
            "sensors=ds18b20%3A0&sensors=ds18b20%3A3",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let (rows, _total) = state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some("display_lines_set".to_string()),
                ..Default::default()
            })
            .await
            .expect("audit page");
        assert_eq!(rows.len(), 1, "{rows:?}");
        let details: serde_json::Value =
            serde_json::from_str(rows[0].details.as_deref().unwrap_or("{}")).expect("json");
        assert_eq!(details["sensors"][0], "ds18b20:0");
        assert_eq!(details["sensors"][1], "ds18b20:3");
    }

    /// The tab renders the controls for a manager and not for a viewer
    /// (§7: an element a role cannot use is not rendered at all).
    #[tokio::test]
    async fn the_unit_tab_shows_the_display_controls_only_to_a_manager() {
        let (app, _dir, _admin, state) = hub_app(Some("hub1")).await;
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;

        let html = body_of(get(&app, "/hubs/hub1/unit", &manager).await).await;
        for path in [
            "/hubs/hub1/display/buzzer-volume",
            "/hubs/hub1/display/screen-brightness",
            "/hubs/hub1/display/led-brightness",
            "/hubs/hub1/display/screen-timeout",
            "/hubs/hub1/display/device-label",
            "/hubs/hub1/display/sensors",
        ] {
            assert!(html.contains(path), "manager sees {path}");
        }
        assert!(html.contains(r#"name="sensors""#), "the picker: {html}");
        // Admin-only actions stay out of a manager's tab.
        assert!(!html.contains("/hubs/hub1/system/power-off"));

        let html = body_of(get(&app, "/hubs/hub1/unit", &viewer).await).await;
        assert!(
            !html.contains("/hubs/hub1/display/"),
            "viewer sees none of it"
        );
    }

    /// The picker fragment serves the same block the tab renders, and its
    /// boxes pre-check from the device's own `config/state` echo.
    #[tokio::test]
    async fn the_picker_fragment_pre_checks_what_the_device_reports() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        state.store.apply(IngestEvent::ConfigState {
            hostname: "hub1".to_string(),
            payload: fiber_protocol::payloads::config_state::ConfigStateMsg {
                display_lines: Some(vec![fiber_protocol::DisplayLine {
                    source: fiber_protocol::DisplayLineSource::Ds18b20,
                    line: Some(2),
                    dev_eui: None,
                    mac: None,
                    field: "temperature".to_string(),
                    label: None,
                    format: fiber_protocol::DisplayLineFormat::default(),
                }]),
                ..Default::default()
            },
        });

        let html = body_of(get(&app, "/hubs/hub1/display/picker", &token).await).await;
        assert!(html.contains(r#"value="ds18b20:2""#), "{html}");
        // Exactly one box is ticked, and the tick sits on that one: the
        // `checked` attribute closes its own tag, so counting `checked>`
        // counts ticked boxes and nothing else.
        assert_eq!(html.matches("checked>").count(), 1, "{html}");
        let ticked = html
            .split("<input")
            .find(|chunk| chunk.contains("checked>"))
            .expect("a ticked box");
        assert!(ticked.contains(r#"value="ds18b20:2""#), "{ticked}");
    }

    /// Stores line 2's thresholds as the Hub echoes them, alarm pair included.
    fn store_line_thresholds(state: &crate::http::AppState) {
        use fiber_protocol::payloads::config_state::{
            ConfigStateMsg, SensorConfigEntry, SensorThresholds,
        };
        let mut msg = ConfigStateMsg::default();
        msg.sensors.push(SensorConfigEntry {
            line: 2,
            name: None,
            location: None,
            enabled: None,
            has_override: None,
            thresholds: Some(SensorThresholds {
                critical_low: Some(-20.0),
                critical_high: Some(10.0),
                alarm_low: Some(-25.0),
                alarm_high: Some(13.7),
                hysteresis: None,
                enabled: None,
                warning_low: Some(-10.0),
                warning_high: Some(5.0),
            }),
        });
        state.store.apply(IngestEvent::ConfigState {
            hostname: "hub1".into(),
            payload: msg,
        });
    }

    /// Confirmed defect (2026-09-26): control-core reads `params.thresholds`
    /// on both its challenge and its dev path and maps a missing bound to
    /// 0.0, so the flat shape set every bound to 0 °C. The four visible
    /// bounds come from the form; the alarm pair, which the UI no longer
    /// shows, is the critical bounds, not the stored -25 / 13.7 (amended
    /// 2026-09-28): a stored or default pair above a sub-zero reading
    /// grades the line Warning.
    /// §13 Q19: only the 1-Wire read interval is saved in the Portal and
    /// pulled by the Hub (`set_sample_interval`, no aggregation, no Hub
    /// window needed); a value out of 1..=60 s is refused before storing.
    #[tokio::test]
    async fn the_read_interval_is_saved_for_the_pull_not_relayed() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        state
            .keys
            .store_pairing(
                "hub1",
                Some("test-ca".into()),
                "00".repeat(32),
                [7u8; 32],
                None,
                None,
            )
            .await
            .expect("store pairing");
        let url = "/hubs/hub1/system/read-interval";
        for form in [
            "sample_interval_s=0",
            "sample_interval_s=61",
            "sample_interval_s=x",
        ] {
            let body = body_of(post(&app, url, &token, form).await).await;
            assert!(body.contains(r#"data-status="error""#), "{form}: {body}");
        }

        let body = body_of(post(&app, url, &token, "sample_interval_s=5").await).await;
        assert!(body.contains(r#"data-status="saved""#), "{body}");
        let published = state.relay.published();
        assert!(
            !published
                .iter()
                .any(|(topic, _)| topic.ends_with("commands/config/request")),
            "not relayed: {published:?}"
        );
        assert!(published
            .iter()
            .any(|(topic, _)| topic.ends_with("config/sync/hint")));
        let entries = crate::sync::desired_entries(&state.db, "hub1")
            .await
            .unwrap();
        let item = entries
            .iter()
            .find(|e| e.key == "hub/sample_interval")
            .expect("item");
        assert_eq!(item.params, json!({ "sample_interval_s": 5 }));
        assert!(!entries.iter().any(|e| e.key == "hub/interval"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_wired_threshold_goes_out_nested_with_the_alarm_pair_on_the_critical_bounds() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        state
            .keys
            .store_pairing(
                "hub1",
                Some("test-ca".into()),
                "00".repeat(32),
                [7u8; 32],
                None,
                None,
            )
            .await
            .expect("store pairing");
        store_line_thresholds(&state);

        post(
            &app,
            "/hubs/hub1/sensors/2/threshold",
            &token,
            "critical_low=-18&warning_low=-8&warning_high=4&critical_high=8&hysteresis=1&enabled=true",
        )
        .await;
        // Cut-over (config_sync.md amendment 2026-10-15 B): nothing is relayed; the Hub
        // pulls the line from the document.
        let published = state.relay.published();
        assert!(
            !published
                .iter()
                .any(|(topic, _)| topic.ends_with("commands/config/request")),
            "a document verb is not relayed: {published:?}"
        );
        let entries = crate::sync::desired_entries(&state.db, "hub1")
            .await
            .unwrap();
        let entry = entries
            .iter()
            .find(|e| e.key == "hub/threshold/2")
            .unwrap_or_else(|| panic!("no document item: {entries:?}"));
        assert_eq!(
            entry.params,
            json!({
                "line": 2,
                "thresholds": {
                    "critical_low": -18.0,
                    "warning_low": -8.0,
                    "warning_high": 4.0,
                    "critical_high": 8.0,
                    "alarm_low": -18.0,
                    "alarm_high": 8.0,
                    "hysteresis": 1.0,
                    "enabled": true,
                }
            })
        );
        // Decision #41: the portal's own row holds the bounds and the hysteresis it grades with.
        let row = state
            .db
            .run(|s| {
                Ok((
                    s.get_field_threshold(
                        "hub1",
                        portal_storage::THRESHOLD_SCOPE_HUB,
                        "2",
                        "temperature",
                    )?,
                    s.field_threshold_hysteresis(
                        "hub1",
                        portal_storage::THRESHOLD_SCOPE_HUB,
                        "2",
                        "temperature",
                    )?,
                ))
            })
            .await
            .expect("read");
        assert_eq!(row.0.expect("a hub row").warning_high, Some(4.0));
        assert_eq!(row.1, Some(1.0));
    }

    /// alarm_hysteresis.md §5: a hysteresis as wide as the narrowest gap between two bounds
    /// is refused, and nothing is sent.
    #[tokio::test]
    async fn a_hysteresis_wider_than_the_band_is_refused() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        store_line_thresholds(&state);
        let html = body_of(
            post(
                &app,
                "/hubs/hub1/sensors/2/threshold",
                &token,
                "critical_low=-18&warning_low=-8&warning_high=4&critical_high=8&hysteresis=4",
            )
            .await,
        )
        .await;
        assert!(html.contains("narrowest gap"), "{html}");
        assert!(state.relay.published().is_empty());
    }

    /// Decision 2026-09-26: the "Alarm" pair is a v1 leftover the device
    /// reads as a second warning bound, never shown in v1. Neither the
    /// Sensors table nor the line's form shows it any more.
    #[tokio::test]
    async fn the_sensors_tab_shows_no_alarm_pair() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        store_line_thresholds(&state);
        let html = body_of(get(&app, "/hubs/hub1/sensors", &token).await).await;
        assert!(html.contains("-20"), "the critical low is shown: {html}");
        assert!(!html.contains("-25"), "the stored alarm low is not: {html}");
        assert!(!html.contains("13.7"), "nor the alarm high: {html}");
        assert!(!html.contains(r#"name="alarm_low""#), "{html}");
        assert!(!html.contains(r#"name="alarm_high""#), "{html}");
        assert!(!html.contains(">Alarm<"), "no Alarm column header: {html}");
    }

    /// v1 required every bound, and control-core turns a missing one into
    /// 0 °C on the device: an empty wired bound is refused in words and
    /// nothing reaches the wire.
    #[tokio::test(start_paused = true)]
    async fn an_empty_wired_bound_is_refused_before_the_wire() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        let response = post(
            &app,
            "/hubs/hub1/sensors/2/threshold",
            &token,
            "critical_low=-18&warning_low=&warning_high=4&critical_high=8",
        )
        .await;
        let html = body_of(response).await;
        assert!(
            html.contains("All four limits are required"),
            "the refusal says why: {html}"
        );
        assert!(state.relay.published().is_empty());
    }

    /// Radio diagnostics §4/§4a: the card renders both documents — the LoRaWAN rows in
    /// place of the v1 chips, the northbridge row with its firmware, reset and last error —
    /// and a `disabled` northbridge hides its row instead of greying it.
    #[tokio::test]
    async fn the_radio_card_renders_the_diagnostics_documents() {
        use crate::state::events::RadioDiagKind;

        let (app, _dir, token, state) = hub_app(None).await;
        state.store.apply(IngestEvent::RadioDiagnostics {
            hostname: "hub1".into(),
            kind: RadioDiagKind::Northbridge,
            doc: Some(json!({
                "timestamp": "2026-09-26T12:00:00Z", "state": "error",
                "fw_version": "0.2.0", "proto_version": 1, "uptime_s": 93_600,
                "boot": { "reset_cause": ["watchdog"], "at": "2026-09-25T12:00:02Z" },
                "radio": { "state": "rx", "freq_hz": 869_525_000, "sf": 7, "tx_power_dbm": 14 },
                "counters": { "rx": 1520, "tx": 1498 },
                "err_flags": ["rx_rearm"],
                "last_error": { "at": "2026-09-26T10:00:00Z", "source": "modem",
                                "code": 2, "message": "RX re-arm failed" }
            })),
        });
        state.store.apply(IngestEvent::RadioDiagnostics {
            hostname: "hub1".into(),
            kind: RadioDiagKind::Lorawan,
            doc: Some(json!({
                "timestamp": "2026-09-26T12:00:00Z", "state": "error",
                "card": { "present": false, "device": "/dev/rak5146" },
                "concentratord": { "unit": "failed", "result": "exit-code", "restarts": 3 },
                "forwarder": { "unit": "active", "restarts": 0, "conn": "offline" },
                "chirpstack": { "unit": "active", "restarts": 0, "api": "ok", "version": "4.19.1" },
                "last_error": { "at": "2026-09-26T12:00:00Z", "message": "card: /dev/rak5146 missing" }
            })),
        });

        let html = body_of(get(&app, "/hubs/hub1/radio-card", &token).await).await;
        for want in [
            ">LoRa P2P</span>",
            "FW 0.2.0 (proto 1)",
            "watchdog",
            "RX re-arm failed",
            "(#2)",
            "869.525 MHz",
            "failed (exit-code) · ↻3",
            "OFFLINE",
            "4.19.1",
            // `/` is HTML-escaped, so the part of the message after the path.
            "rak5146 missing",
            "badge-status-critical",
        ] {
            assert!(html.contains(want), "missing {want:?} in {html}");
        }
        assert!(
            !html.contains("concentratord ✓") && !html.contains("concentratord ✕"),
            "the LoRaWAN rows replace the v1 chips: {html}"
        );

        state.store.apply(IngestEvent::RadioDiagnostics {
            hostname: "hub1".into(),
            kind: RadioDiagKind::Northbridge,
            doc: Some(json!({ "timestamp": "2026-09-26T12:01:00Z", "state": "disabled" })),
        });
        let html = body_of(get(&app, "/hubs/hub1/radio-card", &token).await).await;
        assert!(!html.contains(">LoRa P2P</span>"), "{html}");
    }

    /// The TOWER central's document (#470 P2, the central's own sample): FW with its
    /// commit, no proto, the gateway mode labelled, net / Nodes / queue, the curated counters
    /// with `tx_refused` at 0 — an unknown state verbatim rather than an i18n key, and a
    /// `starting` document before `GET_INFO` with no numbers it does not have.
    #[tokio::test]
    async fn the_radio_card_renders_the_tower_northbridge_document() {
        use crate::state::events::RadioDiagKind;

        let (app, _dir, token, state) = hub_app(None).await;
        let mut doc: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/northbridge_retained_sample.json"
        ))
        .unwrap();
        let nb = |doc: serde_json::Value| IngestEvent::RadioDiagnostics {
            hostname: "hub1".into(),
            kind: RadioDiagKind::Northbridge,
            doc: Some(doc),
        };
        state.store.apply(nb(doc.clone()));
        let html = body_of(get(&app, "/hubs/hub1/radio-card", &token).await).await;
        for want in [
            ">LoRa P2P</span>",
            "FW 0.3.0 · <span",
            "c05b8d3e",
            "gateway receiving · 869.525 MHz · SF7 · 14 dBm",
            "net 1831111266 · Nodes 2/64 · queue 1/32",
            "tx_refused</span> 0",
            "rx_mic_fail</span> 104",
            "rx_dup</span> 102",
            "badge-status-normal",
        ] {
            assert!(html.contains(want), "missing {want:?} in {html}");
        }
        // northbridge_update.md §3.5: running against bundled, on the System tab's
        // Northbridge firmware card since 2026-10-15, not in the Radio card.
        assert!(!html.contains("data-nb-update="), "{html}");
        let fw = body_of(get(&app, "/hubs/hub1/nb-firmware", &token).await).await;
        for want in [
            "data-nb-update=\"current\"",
            "up to date",
            "data-nb-bundled>0.3.0+c05b8d3e<",
        ] {
            assert!(fw.contains(want), "missing {want:?} in {fw}");
        }
        for unwanted in [
            "proto",
            "data-nb-build-flags",
            "evt_resent",
            "rx_fresh",
            "monitoring.hubs.",
        ] {
            assert!(!html.contains(unwanted), "{unwanted:?} in {html}");
        }

        doc["state"] = "rebooting".into();
        doc["radio"]["state"] = "listening".into();
        state.store.apply(nb(doc));
        let html = body_of(get(&app, "/hubs/hub1/radio-card", &token).await).await;
        assert!(html.contains(">rebooting</span>"), "{html}");
        assert!(html.contains("listening · 869.525 MHz"), "{html}");
        assert!(!html.contains("monitoring.hubs."), "{html}");

        let mut starting: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/northbridge_disabled_sample.json"
        ))
        .unwrap();
        starting["state"] = "starting".into();
        state.store.apply(nb(starting.clone()));
        let html = body_of(get(&app, "/hubs/hub1/radio-card", &token).await).await;
        assert!(html.contains(">LoRa P2P</span>"), "{html}");
        for unwanted in [
            "data-nb-fw",
            "data-nb-gateway",
            "MHz",
            "<details",
            ">none",
            "SFnone",
        ] {
            assert!(!html.contains(unwanted), "{unwanted:?} in {html}");
        }

        starting["state"] = "disabled".into();
        state.store.apply(nb(starting));
        let html = body_of(get(&app, "/hubs/hub1/radio-card", &token).await).await;
        assert!(!html.contains(">LoRa P2P</span>"), "{html}");
    }

    /// PN-2 (P2P night test 2026-09-26): in `radio.mode` p2p the `radio/lorawan` document says
    /// `disabled`, ChirpStack is off by design and the `radio/gateways` report says the API is
    /// unreachable. None of that is a fault, so the card shows the one neutral "LoRaWAN off"
    /// line — no API banner, no gateway table, no "no gateway" chip.
    #[tokio::test]
    async fn a_disabled_lorawan_hides_the_gateway_section() {
        use crate::state::events::RadioDiagKind;
        use crate::state::radio::{ApiStatus, GatewaysReport};

        let (app, _dir, token, state) = hub_app(None).await;
        state.store.apply(IngestEvent::RadioGateways {
            report: Some(GatewaysReport {
                timestamp: "2026-09-26T21:50:00Z".into(),
                api_status: ApiStatus::Unreachable,
                gateways: Vec::new(),
            }),
        });
        let html = body_of(get(&app, "/hubs/hub1/radio-card", &token).await).await;
        assert!(
            html.contains("data-radio-banner"),
            "the banner without the document: {html}"
        );

        state.store.apply(IngestEvent::RadioDiagnostics {
            hostname: "hub1".into(),
            kind: RadioDiagKind::Lorawan,
            doc: Some(json!({ "timestamp": "2026-09-26T21:50:00Z", "state": "disabled" })),
        });
        let html = body_of(get(&app, "/hubs/hub1/radio-card", &token).await).await;
        assert!(html.contains(r#"data-lorawan="off""#), "{html}");
        assert!(
            !html.contains("data-radio-banner"),
            "no API banner in p2p: {html}"
        );
        assert!(!html.contains("<table"), "no gateway table in p2p: {html}");
    }

    // ---- „Aktualizovat firmware NB“ (northbridge_update.md §3.4, amended 2026-10-09) ----

    /// The TOWER sample with `update` replaced; `None` leaves the document out.
    fn nb_doc(update: serde_json::Value) -> serde_json::Value {
        let mut doc: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/northbridge_retained_sample.json"
        ))
        .unwrap();
        doc["update"] = update;
        doc
    }

    fn apply_nb(state: &crate::http::AppState, doc: serde_json::Value) {
        state.store.apply(IngestEvent::RadioDiagnostics {
            hostname: "hub1".into(),
            kind: crate::state::events::RadioDiagKind::Northbridge,
            doc: Some(doc),
        });
    }

    fn idle_update() -> serde_json::Value {
        json!({ "state": "idle", "bundled": "0.3.0+c05b8d3e", "attempts": 0,
                "last_error": null, "note": null })
    }

    async fn nb_update_audit_rows(
        state: &crate::http::AppState,
    ) -> Vec<portal_storage::AuditEntry> {
        state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some("hub_northbridge_update".to_string()),
                ..Default::default()
            })
            .await
            .expect("audit page")
            .0
    }

    /// The button is an admin's, on a firmware line whose image bundles firmware and that is
    /// not flashing; the dialog with both versions is on the System tab beside the card.
    /// No bundle (`update: null`), no P2P (`disabled`) or a flash under way: no button.
    #[tokio::test]
    async fn the_nb_update_button_is_offered_to_an_admin_only_where_it_can_act() {
        let (app, _dir, admin, state) = hub_app(None).await;
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        apply_nb(&state, nb_doc(idle_update()));

        let html = body_of(get(&app, "/hubs/hub1/nb-firmware", &admin).await).await;
        assert!(html.contains("data-nb-update-button"), "{html}");
        assert!(html.contains(&tr_html("monitoring.hubs.fw_update_button")));
        let page = body_of(get(&app, "/hubs/hub1/system", &admin).await).await;
        assert!(page.contains("id=\"nb-update-dialog\""), "{page}");
        assert!(page.contains("/hubs/hub1/radio/northbridge-update"));
        assert!(page.contains("name=\"force\""));
        assert!(
            page.contains("data-nb-dialog-running>0.3.0+c05b8d3e<"),
            "{page}"
        );
        assert!(
            page.contains("data-nb-dialog-bundled>0.3.0+c05b8d3e<"),
            "{page}"
        );

        for token in [&manager] {
            let html = body_of(get(&app, "/hubs/hub1/nb-firmware", token).await).await;
            assert!(!html.contains("data-nb-update-button"), "{html}");
            let page = body_of(get(&app, "/hubs/hub1/system", token).await).await;
            assert!(!page.contains("nb-update-dialog"));
        }

        for (case, doc) in [
            ("no bundle", nb_doc(serde_json::Value::Null)),
            (
                "flashing",
                nb_doc(json!({ "state": "flashing", "bundled": "0.3.0+c05b8d3e", "attempts": 1 })),
            ),
            (
                "no P2P",
                json!({ "timestamp": "2026-09-26T12:01:00Z", "state": "disabled" }),
            ),
        ] {
            apply_nb(&state, doc);
            let html = body_of(get(&app, "/hubs/hub1/nb-firmware", &admin).await).await;
            assert!(!html.contains("data-update-enabled"), "{case}: {html}");
            // The dialog stays beside a flashing row: the button comes back when it is over.
            let page = body_of(get(&app, "/hubs/hub1/system", &admin).await).await;
            assert_eq!(
                page.contains("id=\"nb-update-dialog\""),
                case == "flashing",
                "{case}"
            );
        }
    }

    /// The signed `northbridge-update {force}` at level admin, one audit row with `force` and
    /// both versions, and the card back out of band following the update: „Probíhá
    /// aktualizace…“, polling itself with the phase in its query.
    #[tokio::test]
    async fn a_northbridge_update_is_relayed_signed_audited_and_followed() {
        for (body, force) in [("force=on", true), ("", false)] {
            let (app, _dir, _radio, token, state) = radio_mode_app(control_radio::Mode::P2p).await;
            apply_nb(&state, nb_doc(idle_update()));
            let response = with_signed_successes(&state, 1, move || async move {
                post(&app, "/hubs/hub1/radio/northbridge-update", &token, body).await
            })
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let html = body_of(response).await;
            assert!(html.contains("id=\"command-status\""), "{html}");
            assert!(
                html.contains("id=\"hub-nb-firmware\"") && html.contains("hx-swap-oob=\"true\""),
                "the card rides along out of band: {html}"
            );
            assert!(html.contains("data-nb-progress=\"running\""), "{html}");
            assert!(html.contains(&tr_html("monitoring.hubs.nb_update_progress")));
            assert!(html.contains("every 3s"), "{html}");
            assert!(
                html.contains("/hubs/hub1/nb-firmware?nbu=wait&amp;since="),
                "{html}"
            );
            assert!(!html.contains("data-update-enabled"), "not twice: {html}");

            let requests: Vec<serde_json::Value> = state
                .relay
                .published()
                .into_iter()
                .filter(|(topic, _)| topic.ends_with("commands/config/request"))
                .map(|(_, body)| body)
                .collect();
            assert_eq!(requests.len(), 1, "{requests:?}");
            assert_eq!(requests[0]["command_type"], "northbridge-update");
            assert_eq!(requests[0]["level"], "admin");
            assert_eq!(requests[0]["params"], json!({ "force": force }));

            let rows = nb_update_audit_rows(&state).await;
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].target.as_deref(), Some("hub1"));
            assert!(rows[0].success);
            let details: serde_json::Value =
                serde_json::from_str(rows[0].details.as_deref().unwrap_or("{}")).expect("json");
            assert_eq!(details["command"], "northbridge-update");
            assert_eq!(details["force"], force);
            assert_eq!(details["bundled"], "0.3.0+c05b8d3e");
            assert_eq!(details["running"], "0.3.0+c05b8d3e");
        }
    }

    /// Refused before anything is signed where the card offers nothing: no Northbridge row, no
    /// bundled firmware, a flash already under way. Admin only: a manager gets 403.
    #[tokio::test]
    async fn a_northbridge_update_is_refused_where_the_card_offers_none() {
        let (app, _dir, _radio, token, state) = radio_mode_app(control_radio::Mode::P2p).await;
        let post_it = || post(&app, "/hubs/hub1/radio/northbridge-update", &token, "");

        let html = body_of(post_it().await).await;
        assert!(
            html.contains(&tr_html("monitoring.hubs.nb_update_unavailable")),
            "{html}"
        );
        apply_nb(&state, nb_doc(serde_json::Value::Null));
        let html = body_of(post_it().await).await;
        assert!(
            html.contains(&tr_html("monitoring.hubs.nb_update_unavailable")),
            "{html}"
        );
        apply_nb(
            &state,
            nb_doc(json!({ "state": "flashing", "bundled": "0.3.0+c05b8d3e", "attempts": 1 })),
        );
        let html = body_of(post_it().await).await;
        assert!(
            html.contains(&tr_html("monitoring.hubs.nb_update_already_running")),
            "{html}"
        );
        assert!(state.relay.published().is_empty());

        apply_nb(&state, nb_doc(idle_update()));
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let response = post(&app, "/hubs/hub1/radio/northbridge-update", &manager, "").await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(state.relay.published().is_empty());
        assert!(nb_update_audit_rows(&state).await.is_empty());
    }

    /// The card's own refetch carries the update forward: flashing keeps it polling in `run`,
    /// and back on the bundled image it shows the version and stops polling.
    #[tokio::test]
    async fn the_followed_card_reports_the_new_version_and_stops_polling() {
        let (app, _dir, token, state) = hub_app(None).await;
        let since = time::OffsetDateTime::now_utc().unix_timestamp() - 5;
        apply_nb(
            &state,
            nb_doc(json!({ "state": "flashing", "bundled": "0.3.0+c05b8d3e", "attempts": 1 })),
        );
        let uri = format!("/hubs/hub1/nb-firmware?nbu=wait&since={since}");
        let html = body_of(get(&app, &uri, &token).await).await;
        assert!(html.contains("data-nb-progress=\"running\""), "{html}");
        assert!(
            html.contains(&format!("?nbu=run&amp;since={since}")),
            "{html}"
        );
        assert!(html.contains("every 3s"));

        apply_nb(&state, nb_doc(idle_update()));
        let uri = format!("/hubs/hub1/nb-firmware?nbu=run&since={since}");
        let html = body_of(get(&app, &uri, &token).await).await;
        assert!(html.contains("data-nb-progress=\"ok\""), "{html}");
        assert!(html.contains(
            &tr_html("monitoring.hubs.nb_update_done_ok").replace("{version}", "0.3.0+c05b8d3e")
        ));
        assert!(!html.contains("every 3s"), "{html}");
        assert!(
            html.contains("data-nb-update-button"),
            "offered again: {html}"
        );

        // A plain refetch follows nothing.
        let html = body_of(get(&app, "/hubs/hub1/nb-firmware", &token).await).await;
        assert!(!html.contains("data-nb-progress"), "{html}");
    }

    // ---- „Aktualizovat firmware SB“ (southbridge_update.md §3.7) ----

    const SB_BUNDLED: &str = "south-zephyr 1.1.0 g73f39ed5";
    const SB_OLD: &str = "south-zephyr 1.0.0 g11111111";

    /// `system/info` with a `southbridge` block of `version`, the bundled image and an
    /// `update` in `state` (`None`: no update object).
    fn apply_sb(state: &crate::http::AppState, update: Option<serde_json::Value>) {
        let mut sb = json!({ "version": SB_OLD, "bundled": SB_BUNDLED });
        if let Some(update) = update {
            sb["update"] = update;
        }
        let payload: fiber_protocol::payloads::system_info::SystemInfoMsg =
            serde_json::from_value(json!({ "southbridge": sb })).expect("system info");
        state.store.apply(IngestEvent::SystemInfo {
            hostname: "hub1".into(),
            payload: Box::new(payload),
        });
    }

    fn sb_update(state: &str) -> serde_json::Value {
        json!({ "state": state, "attempts": 0 })
    }

    async fn sb_update_audit_rows(
        state: &crate::http::AppState,
    ) -> Vec<portal_storage::AuditEntry> {
        state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some("hub_southbridge_update".to_string()),
                ..Default::default()
            })
            .await
            .expect("audit page")
            .0
    }

    /// The System tab shows the "Firmware SB" card with both versions, the state and the last
    /// error; "—" when the Hub has not reported the block.
    #[tokio::test]
    async fn the_system_tab_shows_the_southbridge_firmware() {
        let (app, _dir, admin, state) = hub_app(None).await;
        let page = body_of(get(&app, "/hubs/hub1/system", &admin).await).await;
        assert!(page.contains("id=\"hub-sb-firmware\""), "{page}");
        assert!(page.contains("data-sb-running>—<"), "{page}");
        assert!(!page.contains("data-update-enabled"));

        apply_sb(
            &state,
            Some(json!({ "state": "failed", "attempts": 1, "last_error": "no ACK after erase" })),
        );
        let page = body_of(get(&app, "/hubs/hub1/system", &admin).await).await;
        assert!(
            page.contains(&format!("data-sb-running>{SB_OLD}<")),
            "{page}"
        );
        assert!(
            page.contains(&format!("data-sb-bundled>{SB_BUNDLED}<")),
            "{page}"
        );
        assert!(page.contains("data-sb-state=\"failed\""), "{page}");
        assert!(page.contains("no ACK after erase"), "{page}");
        assert!(page.contains(&tr_html("monitoring.hubs.sb_fw_state_failed")));
        // The same card on its own refetch.
        let card = body_of(get(&app, "/hubs/hub1/sb-firmware", &admin).await).await;
        assert!(card.contains("sse:device-hub1-status"), "{card}");
        assert!(card.contains("no ACK after erase"), "{card}");
    }

    /// The button is an admin's, where the Hub reports an update object and a bundled image
    /// and no update is running; the dialog with both versions sits beside the card.
    #[tokio::test]
    async fn the_sb_update_button_is_offered_to_an_admin_only_where_it_can_act() {
        let (app, _dir, admin, state) = hub_app(None).await;
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        apply_sb(&state, Some(sb_update("idle")));

        let page = body_of(get(&app, "/hubs/hub1/system", &admin).await).await;
        assert!(page.contains("data-sb-update-button"), "{page}");
        assert!(page.contains(&tr_html("monitoring.hubs.sb_update_button")));
        assert!(page.contains("id=\"sb-update-dialog\""), "{page}");
        assert!(page.contains("/hubs/hub1/system/southbridge-update"));
        assert!(page.contains(&tr_html("monitoring.hubs.sb_update_force")));
        assert!(
            page.contains(&format!("data-sb-dialog-running>{SB_OLD}<")),
            "{page}"
        );
        assert!(
            page.contains(&format!("data-sb-dialog-bundled>{SB_BUNDLED}<")),
            "{page}"
        );

        let page = body_of(get(&app, "/hubs/hub1/system", &manager).await).await;
        assert!(!page.contains("data-sb-update-button"), "{page}");
        assert!(!page.contains("sb-update-dialog"));
        assert!(
            page.contains("id=\"hub-sb-firmware\""),
            "a manager still sees the line"
        );

        apply_sb(&state, Some(sb_update("running")));
        let card = body_of(get(&app, "/hubs/hub1/sb-firmware", &admin).await).await;
        assert!(!card.contains("data-update-enabled"), "running: {card}");
        apply_sb(&state, None);
        let card = body_of(get(&app, "/hubs/hub1/sb-firmware", &admin).await).await;
        assert!(
            !card.contains("data-update-enabled"),
            "no update object: {card}"
        );
        let page = body_of(get(&app, "/hubs/hub1/system", &admin).await).await;
        assert!(!page.contains("id=\"sb-update-dialog\""), "{page}");
    }

    /// The signed `southbridge-update {force}` at level admin, one audit row with `force` and
    /// both versions, and the card back out of band following the update.
    #[tokio::test]
    async fn a_southbridge_update_is_relayed_signed_audited_and_followed() {
        for (body, force) in [("force=on", true), ("", false)] {
            let (app, _dir, _radio, token, state) = radio_mode_app(control_radio::Mode::P2p).await;
            apply_sb(&state, Some(sb_update("idle")));
            let response = with_signed_successes(&state, 1, move || async move {
                post(&app, "/hubs/hub1/system/southbridge-update", &token, body).await
            })
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let html = body_of(response).await;
            assert!(html.contains("id=\"command-status\""), "{html}");
            assert!(
                html.contains("id=\"hub-sb-firmware\"") && html.contains("hx-swap-oob=\"true\""),
                "the card rides along out of band: {html}"
            );
            assert!(html.contains("data-sb-progress=\"running\""), "{html}");
            assert!(html.contains(&tr_html("monitoring.hubs.sb_update_progress")));
            assert!(html.contains("every 3s"), "{html}");
            assert!(
                html.contains("/hubs/hub1/sb-firmware?sbu=wait&amp;since="),
                "{html}"
            );
            assert!(!html.contains("data-update-enabled"), "not twice: {html}");

            let requests: Vec<serde_json::Value> = state
                .relay
                .published()
                .into_iter()
                .filter(|(topic, _)| topic.ends_with("commands/config/request"))
                .map(|(_, body)| body)
                .collect();
            assert_eq!(requests.len(), 1, "{requests:?}");
            assert_eq!(requests[0]["command_type"], "southbridge-update");
            assert_eq!(requests[0]["level"], "admin");
            assert_eq!(requests[0]["params"], json!({ "force": force }));

            let rows = sb_update_audit_rows(&state).await;
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].target.as_deref(), Some("hub1"));
            assert!(rows[0].success);
            let details: serde_json::Value =
                serde_json::from_str(rows[0].details.as_deref().unwrap_or("{}")).expect("json");
            assert_eq!(details["command"], "southbridge-update");
            assert_eq!(details["force"], force);
            assert_eq!(details["bundled"], SB_BUNDLED);
            assert_eq!(details["running"], SB_OLD);
        }
    }

    /// Refused before anything is signed where the card offers nothing: no block, no update
    /// object, an update already running. Admin only: a manager gets 403.
    #[tokio::test]
    async fn a_southbridge_update_is_refused_where_the_card_offers_none() {
        let (app, _dir, _radio, token, state) = radio_mode_app(control_radio::Mode::P2p).await;
        let post_it = || post(&app, "/hubs/hub1/system/southbridge-update", &token, "");

        let html = body_of(post_it().await).await;
        assert!(
            html.contains(&tr_html("monitoring.hubs.sb_update_unavailable")),
            "{html}"
        );
        apply_sb(&state, None);
        let html = body_of(post_it().await).await;
        assert!(
            html.contains(&tr_html("monitoring.hubs.sb_update_unavailable")),
            "{html}"
        );
        apply_sb(&state, Some(sb_update("running")));
        let html = body_of(post_it().await).await;
        assert!(
            html.contains(&tr_html("monitoring.hubs.sb_update_already_running")),
            "{html}"
        );
        assert!(state.relay.published().is_empty());

        apply_sb(&state, Some(sb_update("idle")));
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let response = post(&app, "/hubs/hub1/system/southbridge-update", &manager, "").await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(state.relay.published().is_empty());
        assert!(sb_update_audit_rows(&state).await.is_empty());
    }

    /// The card's own refetch carries the update forward: `running` keeps it polling in `run`;
    /// `idle` with `last_result: ok …` shows the version and stops polling; `failed` the error.
    #[tokio::test]
    async fn the_followed_sb_card_reports_the_outcome_and_stops_polling() {
        let (app, _dir, token, state) = hub_app(None).await;
        let since = time::OffsetDateTime::now_utc().unix_timestamp() - 5;
        apply_sb(&state, Some(sb_update("running")));
        let uri = format!("/hubs/hub1/sb-firmware?sbu=wait&since={since}");
        let html = body_of(get(&app, &uri, &token).await).await;
        assert!(html.contains("data-sb-progress=\"running\""), "{html}");
        assert!(
            html.contains(&format!("?sbu=run&amp;since={since}")),
            "{html}"
        );
        assert!(html.contains("every 3s"));

        apply_sb(
            &state,
            Some(
                json!({ "state": "idle", "attempts": 0, "last_result": format!("ok {SB_BUNDLED}") }),
            ),
        );
        let uri = format!("/hubs/hub1/sb-firmware?sbu=run&since={since}");
        let html = body_of(get(&app, &uri, &token).await).await;
        assert!(html.contains("data-sb-progress=\"ok\""), "{html}");
        assert!(html
            .contains(&tr_html("monitoring.hubs.sb_update_done_ok").replace("{version}", SB_OLD)));
        assert!(!html.contains("every 3s"), "{html}");
        assert!(
            html.contains("data-sb-update-button"),
            "offered again: {html}"
        );

        apply_sb(
            &state,
            Some(json!({ "state": "failed", "attempts": 0, "last_error": "no ACK after erase" })),
        );
        let html = body_of(get(&app, &uri, &token).await).await;
        assert!(html.contains("data-sb-progress=\"failed\""), "{html}");
        assert!(html.contains("no ACK after erase"));

        // A plain refetch, or the radio card's key, follows nothing.
        let html = body_of(get(&app, "/hubs/hub1/sb-firmware", &token).await).await;
        assert!(!html.contains("data-sb-progress"), "{html}");
        let uri = format!("/hubs/hub1/sb-firmware?nbu=run&since={since}");
        let html = body_of(get(&app, &uri, &token).await).await;
        assert!(!html.contains("data-sb-progress"), "{html}");
    }

    // ---- Device information on a probe (metadata.cs.md §5) --------------

    /// The `commands/config/request` bodies the relay published.
    fn config_requests(state: &crate::http::AppState) -> Vec<serde_json::Value> {
        state
            .relay
            .published()
            .into_iter()
            .filter(|(topic, _)| topic.ends_with("commands/config/request"))
            .map(|(_, body)| body)
            .collect()
    }

    /// Name and location both changed: `set_sensor_name` then
    /// `set_sensor_location`, and the description lands in `device_meta`.
    #[tokio::test]
    async fn a_probe_save_of_name_and_location_sends_name_then_location() {
        // Paired, so the relay signs (`radio_mode_app` stores the pairing).
        let (app, _dir, _radio, token, state) = radio_mode_app(control_radio::Mode::P2p).await;
        let response = with_signed_successes(&state, 2, move || async move {
            post(
                &app,
                "/hubs/hub1/sensors/1/meta",
                &token,
                "name=Inlet&description=On+the+pipe&location=Boiler+room",
            )
            .await
        })
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let requests = config_requests(&state);
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert_eq!(requests[0]["command_type"], "set_sensor_name");
        assert_eq!(requests[0]["params"], json!({ "line": 1, "name": "Inlet" }));
        assert_eq!(requests[1]["command_type"], "set_sensor_location");
        assert_eq!(
            requests[1]["params"],
            json!({ "line": 1, "location": "Boiler room" })
        );
        let row = state
            .db
            .run(|s| s.get_device_meta("hub1", super::PROBE_META_SCOPE, "line-2"))
            .await
            .unwrap()
            .expect("the probe's device_meta row");
        assert_eq!(row.description.as_deref(), Some("On the pipe"));
    }

    /// Only the name changed: the single `set_sensor_name`, which an older
    /// agent still understands.
    #[tokio::test]
    async fn a_probe_save_of_the_name_alone_keeps_the_single_command() {
        // Paired, so the relay signs (`radio_mode_app` stores the pairing).
        let (app, _dir, _radio, token, state) = radio_mode_app(control_radio::Mode::P2p).await;
        let response = with_signed_successes(&state, 1, move || async move {
            post(
                &app,
                "/hubs/hub1/sensors/1/meta",
                &token,
                "name=Inlet&description=&location=",
            )
            .await
        })
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let requests = config_requests(&state);
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0]["command_type"], "set_sensor_name");
    }

    /// Nothing the Hub holds changed: no command at all.
    #[tokio::test]
    async fn a_probe_save_of_the_description_alone_sends_nothing() {
        let (app, _dir, token, state) = hub_app(Some("hub1")).await;
        let response = post(
            &app,
            "/hubs/hub1/sensors/1/meta",
            &token,
            "name=&description=Only+this&location=",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(config_requests(&state).is_empty());
    }

    /// A viewer gets the block as plain text: no form, no pencil
    /// (ui_components.md §1).
    #[tokio::test]
    async fn a_viewer_sees_device_information_without_an_edit_form() {
        let (app, _dir, _token, state) = hub_app(Some("hub1")).await;
        let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
        let html = body_of(get(&app, "/hubs/hub1/overview", &viewer).await).await;
        assert!(html.contains(r#"data-device-info="hub""#), "{html}");
        assert!(html.contains("data-te-view=\"name\""), "{html}");
        assert!(!html.contains("data-te-form"), "{html}");
        assert!(!html.contains("te-pencil"), "{html}");
        assert!(!html.contains(r#"id="device-info-hub-name""#), "{html}");
    }

    /// A manager gets the shared inline edit block: the pencil in the
    /// header, one form with the three fields, a counter only on the
    /// description and no Save/Cancel row under the fields.
    #[tokio::test]
    async fn a_manager_edits_device_information_through_the_pencil() {
        let (app, _dir, _token, state) = hub_app(Some("hub1")).await;
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let html = body_of(get(&app, "/hubs/hub1/overview", &manager).await).await;
        assert!(html.contains("te-pencil"), "{html}");
        assert!(html.contains("data-te-form"), "{html}");
        for id in ["name", "description", "location"] {
            assert!(
                html.contains(&format!(r#"id="device-info-hub-{id}" data-te-in"#)),
                "{id}: {html}"
            );
        }
        assert_eq!(html.matches("te-count").count(), 1, "{html}");
        assert!(!html.contains("di-actions"), "{html}");
    }
}
