//! `GET /beacons`, `GET /beacons/add`, `GET /beacons/{mac}`,
//! `POST /beacons/add`, `POST /beacons/{mac}/remove`,
//! `POST /beacons/{mac}/reconfigure`, `POST /beacons/{mac}/history`,
//! `POST /beacons/{mac}/history/{job_id}/cancel`,
//! `POST /beacons/{mac}/history-auto`, `POST /beacons/{mac}/meta` and
//! `POST /beacons/{mac}/thresholds/{field}`
//! — Beacon (BLE EYE tag) routes (M5-G, B5, B6). Context/wiring contracts:
//! `templates/pages/beacons.html`, `templates/partials/beacon_card.html`
//! and `templates/pages/beacon_detail.html` header comments.
//!
//! B5 (§8 WP5.1) removed `POST /beacons/{mac}/detect`: it probed a tag for
//! EN12830 capability, and there is one kind of Beacon (devices.beacon.md
//! §1). The Hub still implements `beacon-detect` as a device-only arm; the
//! portal's registry (`fiber_protocol::registry`, the fixture's
//! `catalogue.json`) carries **no** row for it, because nothing here
//! publishes it — mqtt/design.md §8 hands the arm's fate to `control.md`.
//!
//! Phase 1 is single-device, same "first entry of the store's `devices`
//! `BTreeMap`" resolution as `routes::node_routes`/`history_routes`.
//!
//! Unlike Nodes, `BeaconState` caches no firmware-echoed threshold list
//! (mqtt.md `eye/sensors` carries no `field_thresholds` block) — Beacon
//! thresholds are DB-only (`portal_storage::field_thresholds`, scope
//! `eye`), so there is no merge-precedence question here the way there is
//! for Nodes (`render::merge_node_thresholds`).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use minijinja::context;
use portal_storage::{NewFieldThreshold, THRESHOLD_SCOPE_EYE};
use serde::Deserialize;
use serde_json::json;

use crate::auth::{CurrentUser, RequireAdmin, RequireManager, Role};
use crate::beacon_history::{self, BeaconHistoryError, BeaconHistoryService};
use crate::http::AppState;
use crate::render;
use crate::state::DeviceState;

use super::command::Cmd;
use super::node_routes::{
    backfill_refused, effective_hysteresis, parse_hysteresis, parse_radio_bounds, save_hysteresis,
    sensor_switch_flash, sensor_switch_redirect, BackfillForm, RadioThresholdForm,
    SensorSwitchForm,
};
use super::{non_empty, render as render_page, tr};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/beacons", get(beacons_page))
        // `/beacons/add` before `/beacons/{mac}` in the file only for
        // reading order — matchit prefers the literal segment either way.
        // One registration, two methods: `every_mutating_route_declares_a_role`
        // cuts a registration's span at the next `.route(`, so the GET and the
        // POST have to share this one (devices.backend.md §22).
        .route("/beacons/add", get(beacon_add_page).post(add_beacon))
        .route("/beacons/{mac}", get(beacon_detail_index))
        .route("/beacons/{mac}/{tab}", get(beacon_detail_page))
        .route("/beacons/{mac}/remove", post(remove_beacon))
        .route("/beacons/{mac}/reconfigure", post(reconfigure_beacon))
        .route(
            "/beacons/{mac}/history",
            get(beacon_history_tab).post(request_beacon_history),
        )
        .route(
            "/beacons/{mac}/history/{job_id}/cancel",
            post(cancel_beacon_history),
        )
        .route("/beacons/{mac}/history-auto", post(set_beacon_history_auto))
        .route("/beacons/{mac}/meta", post(set_beacon_meta))
        .route(
            "/beacons/{mac}/device-config",
            post(set_beacon_device_config),
        )
        .route(
            "/beacons/{mac}/thresholds/{field}",
            post(set_beacon_threshold),
        )
        .route(
            "/beacons/{mac}/sensors/{field}",
            post(set_beacon_sensor_enabled),
        )
}

/// The Hub a Beacon route acts on: the one that has this MAC registered.
///
/// Resolved through [`PortalState::hub_of_beacon`] rather than by taking the
/// first Hub in the store (devices.hub.md §12 E) — a MAC is globally unique,
/// so the owner is a lookup. `None` means no Hub knows it, which each caller
/// turns into its own refusal.
fn hostname_for_known_beacon(state: &AppState, mac: &str) -> Option<String> {
    state
        .store
        .snapshot()
        .hub_of_beacon(mac)
        .map(|(device, _)| device.hostname.clone())
}

/// Every Hub that reports the Beacon, the freshest first. Usually one; a Beacon in range of
/// two Hubs is reported by both (devices.hub.md §12 E). A cluster Beacon adds its registry.
fn hostnames_reporting_beacon(state: &AppState, mac: &str) -> Vec<String> {
    let key = mac.trim().to_uppercase();
    let snapshot = state.store.snapshot();
    let mut hubs: Vec<_> = snapshot
        .devices
        .values()
        .filter(|d| d.ble_sensors.contains_key(&key))
        .map(|d| (d.last_seen, d.hostname.clone()))
        .collect();
    hubs.sort_by_key(|h| std::cmp::Reverse(h.0));
    let mut hostnames: Vec<String> = hubs.into_iter().map(|(_, h)| h).collect();
    // A cluster Beacon is stored under the registry (decision #42), which need not hear it.
    if let Some(cluster) = snapshot
        .beacon_cluster
        .as_ref()
        .filter(|c| c.is_registered(&key))
    {
        if !hostnames.contains(&cluster.server) {
            hostnames.insert(0, cluster.server.clone());
        }
    }
    hostnames
}

/// The Hub a Beacon route acts on when there is no Beacon to resolve from
/// yet — registration, which names a MAC no Hub has registered.
///
/// `?hub=` or the only Hub; see [`PortalState::scoped_hub`] for the rule and
/// why several Hubs without `?hub=` is an error rather than a pick.
fn scoped_hostname(state: &AppState, hub: Option<&str>) -> Option<String> {
    let snapshot = state.store.snapshot();
    let resolved = snapshot
        .scoped_hub(hub)
        .map(|device| device.hostname.clone());
    if resolved.is_none() {
        tracing::warn!(
            hub,
            hubs = snapshot.devices.len(),
            "Hub-scoped Beacon route could not resolve a Hub; pass ?hub=<hostname>"
        );
    }
    resolved
}

/// The Hub a Beacon registration goes to in a cluster (decision #42): `Ok(Some(server))`,
/// `Ok(None)` outside a cluster (the `?hub=` rule applies), or `Err(client)` when the
/// request named a Hub that is not the Server.
fn cluster_registry_for(state: &AppState, wanted: Option<&str>) -> Result<Option<String>, String> {
    let snapshot = state.store.snapshot();
    let Some(cluster) = &snapshot.beacon_cluster else {
        return Ok(None);
    };
    if let Some(wanted) = wanted.map(str::trim).filter(|h| !h.is_empty()) {
        if !wanted.eq_ignore_ascii_case(&cluster.server) {
            return Err(wanted.to_string());
        }
    }
    Ok(Some(
        snapshot
            .beacon_registry()
            .map(|d| d.hostname.clone())
            .unwrap_or_else(|| cluster.server.clone()),
    ))
}

/// `client_hub`'s message, which names both Hubs.
fn client_hub_message(server: &str, client: &str) -> String {
    tr("monitoring.beacons.err_client_hub")
        .replace("{server}", server)
        .replace("{client}", client)
}

/// The Hub a GATT command for a known Beacon goes to (reconfigure, history download):
/// its home Hub in a cluster (decision #42), else the Hub that registered it.
fn gatt_hostname_for_known_beacon(state: &AppState, mac: &str) -> Option<String> {
    state
        .store
        .snapshot()
        .home_hub_of_beacon(mac)
        .map(|device| device.hostname.clone())
}

/// Does this Beacon refuse configuration writes?
///
/// `read_only` is the fifth provisioning state (`control-ble`'s `host.rs`):
/// the Hub read the device and it will not take a profile. §3.5 hides the
/// reconfigure button for one, but hiding a button is a UI courtesy and not
/// a rule — the two server-side callers below enforce it, because a button
/// is not the only way to reach a route and the removal path sends
/// `beacon-provision` without any button at all.
///
/// An unknown MAC answers `false`: the route it guards has its own answer
/// for a Beacon that is not there, and refusing on ignorance would turn a
/// missing snapshot into a permanent refusal.
fn refuses_configuration(state: &AppState, hostname: &str, mac: &str) -> bool {
    state
        .store
        .snapshot()
        // HS-4: `device` lowercases the key the map is stored under.
        .device(hostname)
        .and_then(|d| d.ble_sensors.get(mac))
        .and_then(|b| b.provisioning.as_deref())
        == Some("read_only")
}

// ---------------------------------------------------------------------
// GET /beacons
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct BeaconsQuery {
    ok: Option<String>,
    error: Option<String>,
}

fn beacon_err_message(code: &str) -> Option<String> {
    // A command refused before anything was signed: `not_paired` and its two siblings.
    if let Some(message) = super::command::refusal_message(code) {
        return Some(message);
    }
    let key = match code {
        "invalid_mac" => "monitoring.beacons.err_invalid_mac",
        "no_device" => "monitoring.shared.no_device_yet",
        "choose_hub" => "monitoring.beacons.err_choose_hub",
        "add_failed" => "monitoring.beacons.err_add_failed",
        "already_registered" => "monitoring.beacons.err_already_registered",
        "serial_required" => "monitoring.beacons.err_serial_required",
        "save_failed" => "monitoring.meta.err_save_failed",
        "confirm_mismatch" => "monitoring.beacons.err_remove_confirm_mismatch",
        "remove_failed" => "monitoring.beacons.err_remove_failed",
        "unknown_beacon" => "monitoring.beacons.err_unknown_beacon",
        _ => "monitoring.shared.something_went_wrong",
    };
    Some(tr(key))
}

/// The list's success flash. `?ok=added` has redirected here since B5 and
/// had nowhere to render until removal needed the same lane — the page
/// simply dropped it.
fn beacon_ok_message(code: &str) -> Option<String> {
    let key = match code {
        "added" => "monitoring.beacons.added_ok",
        "removed" => "monitoring.beacons.removed_ok",
        _ => return None,
    };
    Some(tr(key))
}

/// Loads the per-pass extras every Beacon row needs (`device_meta`, stored
/// thresholds, B2's offline verdict, the signal-quality floors) — the
/// Beacon twin of `node_row_data`.
///
/// **Two bulk queries for the whole fleet, run concurrently.** Before B5
/// four separate sites each awaited one `list_field_thresholds` *per
/// Beacon* (§16.3): this function, the dashboard's threshold map, and both
/// `ChangeEvent::Beacon`/`BeaconBatch` arms in `sse.rs`. A read failure
/// degrades to empty rather than failing the page, same rule as the Node
/// side: a Beacon list missing its descriptions is worth serving.
///
/// Takes its dependencies as parts rather than an `AppState` because the
/// SSE loop has the parts and not the state.
pub(crate) async fn beacon_row_data(
    db: &crate::db::Db,
    offline: &crate::offline::OfflineService,
    calibration: &crate::calibration::CalibrationService,
    signal_bands: crate::config::SignalQualityConfig,
    hostname: &str,
) -> render::BeaconRowData {
    let (h1, h2) = (hostname.to_string(), hostname.to_string());
    let (meta, thresholds) = tokio::join!(
        db.run(move |s| s.device_meta_map(&h1, THRESHOLD_SCOPE_EYE)),
        db.run(move |s| s.field_thresholds_by_entity(&h2, THRESHOLD_SCOPE_EYE)),
    );

    render::BeaconRowData {
        meta: meta.unwrap_or_else(|err| {
            tracing::warn!(%err, hostname, "beacon device_meta unreadable; rows render without it");
            Default::default()
        }),
        thresholds: thresholds.unwrap_or_else(|err| {
            tracing::warn!(%err, hostname, "beacon thresholds unreadable; rows render without them");
            Default::default()
        }),
        liveness: offline.liveness_snapshot(),
        signal_bands,
        calibration: calibration.levels_for(hostname, crate::calibration::CalibrationScope::Beacon),
    }
}

pub(crate) async fn load_beacon_row_data(
    state: &AppState,
    hostname: &str,
) -> render::BeaconRowData {
    beacon_row_data(
        &state.db,
        &state.offline,
        &state.calibration,
        state.signal_quality,
        hostname,
    )
    .await
}

/// Every Beacon a Hub knows, in `partials/beacon_card.html`'s `b` shape and
/// registration order. Shared by `GET /beacons` and the dashboard's unified
/// sensor table, so the same row renders identically on both pages.
///
/// Discovered candidates get the same context as registered ones now that
/// building it costs no query: their `device_meta`/threshold lookups are
/// simply misses in maps that were already loaded.
pub(crate) async fn beacon_ctxs(
    state: &AppState,
    portal_state: &crate::state::PortalState,
    device: Option<&DeviceState>,
) -> Vec<render::BeaconCtx> {
    let Some(device) = device else {
        return Vec::new();
    };
    // Decision #42: a cluster lists every Beacon once, under the Server — the registry that
    // keys its metadata and thresholds — whichever Hub hears it. The other Hubs add no rows.
    if let Some(cluster) = &portal_state.beacon_cluster {
        if !device.hostname.eq_ignore_ascii_case(&cluster.server) {
            return Vec::new();
        }
        let rows = load_beacon_row_data(state, &device.hostname).await;
        return portal_state
            .cluster_beacons()
            .unwrap_or_default()
            .iter()
            .map(|beacon| {
                let mut ctx = render::beacon_ctx(
                    rows.inputs(beacon, render::beacon_hubs(portal_state, &beacon.mac)),
                );
                ctx.hostname = Some(device.hostname.clone());
                ctx
            })
            .collect();
    }
    let rows = load_beacon_row_data(state, &device.hostname).await;
    device
        .ble_sensors
        .values()
        .map(|beacon| {
            let mut ctx = render::beacon_ctx(
                rows.inputs(beacon, render::beacon_hubs(portal_state, &beacon.mac)),
            );
            // The row's own Hub, so a list spanning several of them keeps
            // each row's SSE target right (devices.hub.md §12 E).
            ctx.hostname = Some(device.hostname.clone());
            ctx
        })
        .collect()
}

/// The same Beacon thresholds `beacon_ctxs` uses, keyed by MAC — the shape
/// the monitoring page's row builder wants (it resolves a Beacon per group
/// member, so a list would mean a linear scan per row).
///
/// One bulk query, not one per Beacon: this was the second of §16.3's four
/// N+1 sites and it is the one the dashboard pays on every page load. It
/// makes that call itself rather than going through `BeaconRowData`, because
/// what the dashboard wants here is the threshold map and not a rendered
/// `BeaconCtx` — the one of the four sites the shared builder does not
/// cover (B5 review, 2026-09-08).
/// Spans every Hub (devices.hub.md §12 E): the monitoring page's groups are
/// cross-Hub by design, and a MAC is globally unique, so one map keyed by MAC
/// still answers every row. Still one query per Hub, not per Beacon.
pub(crate) async fn beacon_threshold_map(
    state: &AppState,
    portal_state: &crate::state::PortalState,
) -> crate::monitoring::BeaconThresholds {
    let mut out = crate::monitoring::BeaconThresholds::default();
    for device in portal_state.devices.values() {
        let by_entity = ({
            let a0 = device.hostname.clone();
            let a1 = THRESHOLD_SCOPE_EYE.to_string();
            state
                .db
                .run(move |s| s.field_thresholds_by_entity(&a0, &a1))
        })
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, hostname = %device.hostname, "failed to load beacon thresholds");
            Default::default()
        });
        out.extend(
            device
                .ble_sensors
                .keys()
                .filter_map(|mac| by_entity.get(mac).map(|t| (mac.clone(), t.clone()))),
        );
    }
    out
}

async fn beacons_page(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Query(q): Query<BeaconsQuery>,
) -> Response {
    let portal_state = state.store.snapshot();
    // Every Hub's registered Beacons in one list (devices.hub.md §12 E).
    // `hostname` stays the first Hub, for the empty state's message only.
    let hostnames: Vec<String> = portal_state.devices.keys().cloned().collect();
    let hostname = portal_state
        .devices
        .values()
        .next()
        .map(|d| d.hostname.clone());
    let can_edit = render::can_edit_thresholds(&current_user);

    let mut beacons: Vec<render::BeaconCtx> = Vec::new();
    for device in portal_state.devices.values() {
        beacons.extend(beacon_ctxs(&state, &portal_state, Some(device)).await);
    }

    let error = q
        .error
        .and_then(non_empty)
        .and_then(|c| beacon_err_message(&c));
    let ok = q.ok.and_then(non_empty).and_then(|c| beacon_ok_message(&c));
    // GitLab #109: the same unpaired-Hub banner as on the registration form.
    let unpaired = super::unpaired_banner(&state, &current_user, &hostnames, "/beacons").await;

    let ctx = render::page_ctx(
        context! {
            nav_active => "beacons",
            hostname => hostname,
            hostnames => hostnames,
            can_edit => can_edit,
            unpaired => unpaired,
            beacons => beacons,
            ok => ok,
            error => error,
        },
        Some(&current_user),
    );
    render_page(&state, "pages/beacons.html", ctx)
}

// ---------------------------------------------------------------------
// GET /beacons/add — devices.beacon.md §4 (B6, WP6.1)
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct BeaconAddQuery {
    error: Option<String>,
    /// Preselects the MAC, so `/beacons/add?mac=…` is a shareable link to
    /// "register this one". The scan table's own Add button is client-side
    /// and needs no round trip.
    mac: Option<String>,
    /// Which Hub scans and registers — the `?hub=` rule
    /// ([`PortalState::scoped_hub`]). Carried into the form's action so the
    /// POST lands on the same Hub the scan came from.
    hub: Option<String>,
}

/// One Hub's half of the scan table on `/beacons/add` — `partials/discovered_table.html`'s
/// context under the SSE shell named after the Hub.
#[derive(serde::Serialize)]
struct ScanPanelCtx {
    hostname: String,
    d: render::DiscoveredPanelCtx,
}

/// The registration page: the form above, the live passive scan below.
///
/// `CurrentUser` and not `RequireManager` — a viewer may watch the scan (the
/// same rule every read in this module follows); `can_edit` is what hides
/// the form and the Add buttons. `POST /beacons/add` is the gate that
/// matters, and it is `RequireManager`.
async fn beacon_add_page(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Query(q): Query<BeaconAddQuery>,
) -> Response {
    let portal_state = state.store.snapshot();
    // Every Hub scans — a Beacon is a broadcast, and §4's table names the Hub
    // that heard each row — so the page shows one panel per Hub, each its own
    // SSE target. The FORM registers on one Hub: `?hub=`, else the only Hub,
    // else the first by name with a select to change it (devices.hub.md
    // §12 E). Until this the page took `scoped_hub` for both halves, and in a
    // cluster without `?hub=` that meant no scan and a POST with nowhere to go.
    let hostnames: Vec<String> = portal_state.devices.keys().cloned().collect();
    let hostname = portal_state
        .scoped_hub(q.hub.as_deref())
        .map(|d| d.hostname.clone())
        .or_else(|| hostnames.first().cloned());
    let now = render::now_unix();
    // Decision #42: a cluster registers on its Server alone and scans as one — no Hub
    // select, and a single merged panel with one row per stranger MAC.
    let cluster_server = portal_state
        .beacon_cluster
        .as_ref()
        .map(|c| c.server.clone());
    let panels: Vec<ScanPanelCtx> = if cluster_server.is_some() {
        vec![ScanPanelCtx {
            hostname: "cluster".to_string(),
            d: render::cluster_discovered_panel(&portal_state, &state.signal_quality, now),
        }]
    } else {
        portal_state
            .devices
            .values()
            .map(|device| ScanPanelCtx {
                hostname: device.hostname.clone(),
                d: render::discovered_panel(device, &state.signal_quality, now),
            })
            .collect()
    };

    let error = q.error.and_then(non_empty).and_then(|c| {
        if c == "client_hub" {
            return Some(client_hub_message(
                cluster_server.as_deref().unwrap_or("?"),
                q.hub.as_deref().unwrap_or("?"),
            ));
        }
        beacon_err_message(&c)
    });
    let cluster_note = cluster_server
        .as_deref()
        .map(|server| tr("monitoring.beacons.cluster_register_note").replace("{server}", server));

    // GitLab #109: registration is a signed command, so a Hub this portal is not paired with
    // cannot take one. The form says so up front, and is disabled while NO Hub could sign —
    // the single-Hub case; in a cluster the select can still pick a paired one, and the POST
    // names the pairing if it does not.
    let can_edit = render::can_edit_thresholds(&current_user);
    let unpaired = super::unpaired_banner(&state, &current_user, &hostnames, "/beacons/add").await;
    let register_blocked = unpaired.blocks_all(&hostnames);

    let ctx = render::page_ctx(
        context! {
            nav_active => "beacons",
            hostname => hostname,
            hostnames => hostnames,
            can_edit => can_edit,
            unpaired => unpaired,
            register_blocked => register_blocked,
            panels => panels,
            cluster_server => cluster_server,
            cluster_note => cluster_note,
            error => error,
            prefill_mac => q.mac.and_then(non_empty).map(|m| m.trim().to_uppercase()),
        },
        Some(&current_user),
    );
    render_page(&state, "pages/beacon_add.html", ctx)
}

// ---------------------------------------------------------------------
// GET /beacons/{mac} — devices.beacon.md §3 (B5, WP5.2 + WP5.4)
// ---------------------------------------------------------------------

/// Flash codes from the metadata form's redirect, exactly as
/// `node_detail_page` reads them: the `device_meta` write touches SQLite
/// only, so there is no `CommandOutcome` to render into `#command-status`.
#[derive(Deserialize)]
struct BeaconDetailQuery {
    ok: Option<String>,
    error: Option<String>,
    /// View state the page writes back into the URL itself: which field
    /// and window the single chart shows (the tab is in the path). Same three
    /// as `node_detail_page` — the SSE `refresh` event reloads the whole
    /// page, so without them every repaint would throw the operator back to
    /// the default view.
    metric: Option<String>,
    range: Option<String>,
}

/// The Beacon's own page. `CurrentUser`, like `GET /nodes/{key}`: a viewer
/// reads it, `can_edit` gates the two editable halves inside it.
///
/// 404 when the Hub does not know the MAC — the same rule
/// `set_beacon_meta` has enforced since §19 (F7), so a stale browser tab
/// cannot address a Beacon that is gone.
///
/// **One Hub resolution, `hub_of_beacon`.** The page and every write on it
/// resolve the Hub from the MAC (devices.hub.md §12 E), so a two-Hub store
/// can no longer read one Hub's telemetry and write another's `device_meta`.
/// The `beacon_hubs` table below stays display-only and may still list
/// several: hearing a broadcast is not owning the registration.
/// `/beacons/{mac}` and the old `?tab=` links land on a tab's own path.
async fn beacon_detail_index(
    _current_user: CurrentUser,
    Path(mac): Path<String>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Response {
    super::device_tab_redirect(
        &format!("/beacons/{}", mac.trim().to_uppercase()),
        raw.as_deref(),
    )
}

/// GET of the History tab: `/beacons/{mac}/history` is also the history
/// request's POST, and matchit ranks that literal above `{tab}`.
async fn beacon_history_tab(
    current_user: CurrentUser,
    state: State<AppState>,
    Path(mac): Path<String>,
    q: Query<BeaconDetailQuery>,
) -> Response {
    beacon_detail_page(current_user, state, Path((mac, "history".into())), q).await
}

async fn beacon_detail_page(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path((mac, tab)): Path<(String, String)>,
    Query(q): Query<BeaconDetailQuery>,
) -> Response {
    // An unknown tab is a 404, as on the Hub's page.
    if !super::DEVICE_TABS.contains(&tab.as_str()) {
        return super::not_found(&state, Some(&current_user));
    }
    let mac = mac.trim().to_uppercase();
    let portal_state = state.store.snapshot();
    // Resolved off the snapshot already taken, so the page cannot straddle
    // two of them.
    // A MAC no Hub knows — most often one that was just removed and is
    // still open in a tab — goes back to the list with a line saying so,
    // not to a bare-text 404 (the operator asked for the list, 2026-09-23).
    let Some((device, beacon)) = portal_state.hub_of_beacon(&mac) else {
        return Redirect::to("/beacons?error=unknown_beacon").into_response();
    };
    // Heard but not registered — a candidate, or a Beacon just removed that
    // keeps advertising. Its "detail" would offer removal and a
    // configuration state for a registration that does not exist; what the
    // operator can do with it is register it, so that is where it goes,
    // with the MAC and the Hub that hears it filled in (decided 2026-09-23).
    // "Not registered" means on no Hub: `hub_of_beacon` prefers a registered
    // copy, so a heard one comes back only when no Hub owns the Beacon.
    if beacon.discovered == Some(true) {
        return Redirect::to(&format!("/beacons/add?mac={mac}&hub={}", device.hostname))
            .into_response();
    }

    // The same bulk load the list does. One Beacon does not need it to be
    // bulk, but it does need the page and the row to agree about liveness,
    // thresholds and metadata, and going through one builder is how that
    // is guaranteed rather than hoped for.
    let rows = load_beacon_row_data(&state, &device.hostname).await;
    let mut b = render::beacon_ctx(rows.inputs(beacon, render::beacon_hubs(&portal_state, &mac)));
    // §3.3's raw-message preview is this page's alone, so it is built here
    // and not in `beacon_ctx`, which every list render and every SSE card
    // frame also goes through (B6 review, item 10).
    b.last_payload_json = render::beacon_last_payload_json(beacon);

    let can_edit = render::can_edit_thresholds(&current_user);
    // Removal is `RequireAdmin` (plan/portal/mqtt/design.md §13 item 3,
    // ratified 2026-09-15): the button follows the route's own gate, not the
    // manager-level `can_edit`, so nobody is shown a control that 403s.
    let can_remove = current_user.has_role(Role::Admin);
    let active_tab = super::DEVICE_TABS
        .into_iter()
        .find(|t| *t == tab)
        .unwrap_or("overview");
    let selected_range = q.range.clone().unwrap_or_else(|| "24h".to_string());
    // #120: the sensor switch's own flash; any other `ok` is the metadata form's.
    let sensor_ok = q.ok.as_deref().and_then(|ok| {
        sensor_switch_flash(ok, |field| {
            let fallback = fiber_protocol::eye_field(field).map_or(field, |f| f.label);
            crate::i18n::field_label(crate::i18n::current_lang(), field, fallback)
        })
    });
    let meta_ok =
        q.ok.and_then(non_empty)
            .filter(|_| sensor_ok.is_none())
            .map(|_| tr("monitoring.meta.saved"));
    // The removal modal's two refusals come back to this page, not the list:
    // the Beacon is still registered, so the operator is where the retry is.
    // They land on the Overview tab, where the metadata form's flash is not
    // drawn, so they get their own alert under the remove button (UX-044).
    let error = q.error.and_then(non_empty);
    let remove_error = match error.as_deref() {
        Some("confirm_mismatch") => Some(tr("monitoring.beacons.err_remove_confirm_mismatch")),
        Some("remove_failed") => Some(tr("monitoring.beacons.err_remove_failed")),
        // Refused before anything was signed — an unpaired Hub above all (#109).
        Some(code) => super::command::refusal_message(code),
        None => None,
    };
    let meta_error = error
        .filter(|_| remove_error.is_none())
        .map(|code| match code.as_str() {
            "description_too_long" => tr("monitoring.meta.err_description_too_long"),
            "name_too_long" => tr("monitoring.meta.err_name_too_long"),
            "invalid_offline_k" => tr("monitoring.shared.count_must_be_number")
                .replace("{field}", &tr("monitoring.meta.offline_k_label")),
            _ => tr("monitoring.meta.err_save_failed"),
        });
    // The History tab's table (beacon_history.md §4.3), read only for that
    // tab, and the Settings tab's switch (a missing row reads as on, §4.2).
    let (jobs, gaps) = if active_tab == "history" {
        tokio::join!(
            state
                .beacon_history
                .jobs_ctx(&state.store, &device.hostname, &mac),
            state.beacon_history.gaps_ctx(&device.hostname, &mac),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let history_auto = b.meta.backfill_auto;

    // The Overview tab's calibration card (calibration.cs.md §2), read only
    // for that tab; `None` for a discovered candidate, which is not a sensor
    // yet.
    let cal = match super::calibration_routes::Target::beacon(&state, &mac) {
        Some(target) if active_tab == "overview" => {
            Some(super::calibration_routes::card_ctx(&state, &target, &current_user).await)
        }
        _ => None,
    };

    // The Overview's Channels card (ui_components.md §2).
    let channels = if active_tab == "overview" {
        Some(super::channels::beacon_card(
            format!("channels-{}", mac.replace(':', "")),
            &mac,
            &b.fields,
            b.thresholds.as_ref(),
            beacon.battery_mv,
            beacon.rssi,
            b.home_hub.as_deref(),
            beacon.last_seen,
            can_edit,
            render::now_unix(),
        ))
    } else {
        None
    };

    // "Device information" (metadata.cs.md §5) opens the Overview.
    let device_info = (active_tab == "overview").then(|| {
        let mut di = render::beacon_info_ctx(&b, cal.as_ref(), can_edit);
        di.ok = meta_ok.clone();
        di.error = meta_error.clone();
        di
    });
    // The Diagnostics tab's Alarm messages and Message history cards
    // (ui_components.md, amendment 2026-10-15). A Beacon keeps no message
    // ring (§3.3): its history is the one last message.
    let diag = if active_tab == "diagnostics" {
        let filter = portal_storage::AlarmListFilter {
            limit: 50,
            sensor_id_prefix: Some(format!("eye:{mac}:")),
            ..Default::default()
        };
        let alarms = state
            .db
            .run(move |s| s.list_alarms(&filter))
            .await
            .map(|(rows, _)| rows)
            .unwrap_or_else(|err| {
                tracing::warn!(%err, mac, "failed to load the Beacon's alarms");
                Vec::new()
            });
        let messages: Vec<_> = b
            .last_payload_json
            .as_deref()
            .map(|j| render::diag_json_message(b.last_seen.clone(), j))
            .into_iter()
            .collect();
        Some(context! {
            alarms => render::diag_alarm_events(&alarms, 50),
            messages => messages,
        })
    } else {
        None
    };
    let ctx = render::page_ctx(
        context! {
            nav_active => "beacons",
            di => device_info,
            hostname => device.hostname.clone(),
            // GitLab #109: its limits, interval and removal are signed commands to this Hub.
            unpaired => super::unpaired_banner(
                &state,
                &current_user,
                std::slice::from_ref(&device.hostname),
                &format!("/beacons/{mac}"),
            )
            .await,
            has_limits => b.thresholds.as_ref().is_some_and(|t| render::any_bounded(t.values())),
            b => b,
            diag => diag,
            can_edit => can_edit,
            can_remove => can_remove,
            active_tab => active_tab,
            selected_metric => q.metric,
            selected_range => selected_range,
            // WP5.4's "inherits YAML N" line: the effective default this
            // Beacon falls back to when its override is blank.
            offline_k_default => state.beacons.offline_detector,
            // The cluster-wide report (storage) interval, in seconds (#141).
            report_interval_s => u64::from(state.beacons.store_interval) * 60,
            meta_ok => meta_ok,
            // The metadata card's location combobox (metadata.cs.md).
            locations => super::location_suggestions(&state).await,
            sensor_ok => sensor_ok,
            meta_error => meta_error,
            remove_error => remove_error,
            jobs => jobs,
            gaps => gaps,
            history_auto => history_auto,
            backfill_from => crate::display_tz::datetime_local(render::now_unix() - 24 * 3600),
            backfill_to => crate::display_tz::datetime_local(render::now_unix()),
            cal => cal,
            ch => channels,
            evc => super::probe_routes::events_card(&state, &current_user, &format!("beacon:{mac}"), None).await,
        },
        Some(&current_user),
    );
    render_page(&state, "pages/beacon_detail.html", ctx)
}

// ---------------------------------------------------------------------
// POST /beacons/add
// ---------------------------------------------------------------------

/// devices.beacon.md §4's upper half: **SN + MAC + name**.
///
/// No `location`. The spec's wireframe shows one, and there is nowhere
/// honest to put it: `BeaconState::location` comes off the Hub's
/// `config/state`, and `beacon-add` carries `mac` and `name` and nothing
/// else (`control-core`'s `MqttCommand::AddBeaconTag`). A field whose value
/// is silently discarded is worse than no field, so this one is left out and
/// the gap is recorded in §22.4 rather than papered over with a second
/// portal-side location that would then disagree with the Hub's.
#[derive(Deserialize)]
struct AddBeaconForm {
    /// Which Hub registers it — the page's select in a cluster, absent on a
    /// single-Hub portal. `?hub=` on the action is the fallback.
    #[serde(default)]
    hub: Option<String>,
    /// Off the device's label. Required: it is the Beacon's primary identity
    /// in its own detail header (§3.1), and the advertisement carries none
    /// (beacon-btsmp1.md §1), so if it is not typed here it is never known.
    /// Blank by default like `mac` (O-3): a missing field gets the page's flash, not a 422.
    #[serde(default)]
    serial: String,
    #[serde(default)]
    mac: String,
    /// Optional, and blank falls back to the serial — the same rule the Node
    /// dialog follows, so both registration forms behave alike.
    #[serde(default)]
    name: String,
    /// Operator's note. Portal-side only, like the serial below it; there is
    /// no wire field for either.
    #[serde(default)]
    description: String,
}

/// `AA:BB:CC:DD:EE:FF` — 6 colon-separated 2-hex-digit octets. Validated
/// before ever touching the relay (route 6's contract: "mac uppercased").
fn is_valid_mac(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

async fn add_beacon(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Query(hub): Query<super::HubQuery>,
    Form(form): Form<AddBeaconForm>,
) -> Response {
    let mac = form.mac.trim().to_uppercase();
    if !is_valid_mac(&mac) {
        return Redirect::to("/beacons/add?error=invalid_mac").into_response();
    }
    let Some(serial) = non_empty(form.serial) else {
        return Redirect::to("/beacons/add?error=serial_required").into_response();
    };

    // A MAC that is already **registered** on ANY Hub is an error, not a
    // second registration that silently wins (§4: "duplicitní registrace
    // (stejná MAC) → chyba, ne falešný úspěch"). A *discovered* MAC is the
    // normal case — that is what the scan table is for — so the check is on
    // the enrolled half alone. The MAC is globally unique, so the check
    // spans the store (devices.hub.md §12 E).
    let snapshot = state.store.snapshot();
    let registered = match &snapshot.beacon_cluster {
        // In a cluster the Server's registry is the answer — a Client may still list a
        // stranger it has not had a view for, and that is not a registration.
        Some(cluster) => cluster.is_registered(&mac),
        None => snapshot
            .hub_of_beacon(&mac)
            .is_some_and(|(_, b)| b.discovered != Some(true)),
    };
    drop(snapshot);
    if registered {
        return Redirect::to("/beacons/add?error=already_registered").into_response();
    }

    // Registration names a MAC no Hub has registered yet, so there is
    // nothing to resolve the Hub from — this is the `?hub=` rule, see
    // `scoped_hostname`. The form's own field wins over the query; a
    // cluster with neither is asked to choose, not told nothing reported.
    let wanted = form
        .hub
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .or_else(|| hub.hub.clone());
    // Decision #42: a cluster's registry is its Server. No Hub to choose, and a request
    // naming a Client is refused here — the Client would refuse it anyway, but only after
    // a signed round trip and with a message in English.
    let registry = match cluster_registry_for(&state, wanted.as_deref()) {
        Ok(registry) => registry,
        Err(client) => {
            return Redirect::to(&format!(
                "/beacons/add?error=client_hub&hub={}",
                // A hostname, echoed into the message; anything else is dropped rather
                // than escaped.
                client
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
                    .collect::<String>()
            ))
            .into_response();
        }
    };
    let Some(hostname) = registry.or_else(|| scoped_hostname(&state, wanted.as_deref())) else {
        let code = if state.store.snapshot().devices.len() > 1 {
            "choose_hub"
        } else {
            "no_device"
        };
        return Redirect::to(&format!("/beacons/add?error={code}")).into_response();
    };

    let name = non_empty(form.name.trim().to_string()).unwrap_or_else(|| serial.clone());
    let description = non_empty(form.description.trim().to_string());

    let mut params = serde_json::Map::new();
    params.insert("mac".to_string(), json!(mac));
    params.insert("name".to_string(), json!(name));

    // A refusal before anything was signed — above all an unpaired Hub — comes back as its own
    // code: folded into `add_failed` it read "check the MQTT connection" (GitLab #109). Not
    // audited, as no refusal from `Cmd::execute` is: nothing was sent.
    let outcome = match Cmd::new(&state, &current_user, "beacon-add")
        .execute_or_code(&hostname, serde_json::Value::Object(params), None)
        .await
    {
        Ok(outcome) => outcome,
        Err(code) => return Redirect::to(&format!("/beacons/add?error={code}")).into_response(),
    };

    if outcome.status == "success" {
        // The serial is portal-side metadata (V14): it never travelled to
        // the Hub, because there is no wire field for it. Written after the
        // command succeeds, so a refused registration leaves no orphan row
        // for the next device on that MAC to inherit (§19.5's rule, from
        // the other direction).
        let saved = super::edit_device_meta(
            &state,
            &current_user,
            &hostname,
            THRESHOLD_SCOPE_EYE,
            &mac,
            |row| {
                row.serial = Some(serial.clone());
                // Blank leaves the column as it was rather than nulling it,
                // the rule `edit_device_meta`'s carry-over follows.
                if description.is_some() {
                    row.description = description.clone();
                }
            },
        )
        .await;
        if let Err(err) = &saved {
            // The Beacon *is* registered — the Hub said so — so this is not
            // a failed registration, only a serial the portal could not
            // keep. The detail header will say "not recorded" and the
            // operator can add it there.
            tracing::warn!(%err, hostname, mac, "beacon registered but its serial was not stored");
        }
        // Its default bands, switched off (threshold_defaults.md §3): nothing goes to the Hub
        // until an operator switches one on.
        let fields: Vec<&str> = crate::threshold_defaults::beacon_fields().collect();
        super::seed_threshold_defaults(&state, &hostname, THRESHOLD_SCOPE_EYE, &mac, &fields).await;
        // Audited **after** the write, with what actually happened: the row
        // is the Class IIa answer to "was this device's serial captured at
        // registration", and hardcoding `true` before the write made it
        // answer "yes" for a serial that had not been stored and, on a
        // refused registration, for one that was never attempted (B6
        // review, item 3).
        super::audit_action(
            &state,
            &current_user,
            "beacon_add",
            &mac,
            json!({ "status": outcome.status, "serial_recorded": saved.is_ok() }),
            true,
        )
        .await;
        Redirect::to("/beacons?ok=added").into_response()
    } else {
        tracing::warn!(
            hostname,
            mac,
            status = outcome.status,
            "beacon-add did not succeed"
        );
        // No `serial_recorded` key at all: nothing was registered, so there
        // was no serial to record and no claim to make either way.
        super::audit_action(
            &state,
            &current_user,
            "beacon_add",
            &mac,
            json!({ "status": outcome.status }),
            false,
        )
        .await;
        Redirect::to("/beacons/add?error=add_failed").into_response()
    }
}

// ---------------------------------------------------------------------
// POST /beacons/{mac}/meta
// ---------------------------------------------------------------------

/// The Beacon half of `device_meta` (devices.backend.md §3, WP0.2).
///
/// The route shipped ahead of its UI: it was written when Beacons had no
/// detail page, so it redirected to `/beacons`, where nothing rendered a
/// success flash. B5 (§8 WP5.4) gave it the surface it was waiting for and
/// the redirect now goes back to that page with `?ok=`/`?error=`, the same
/// convention `set_node_meta` follows. Same blank-vs-zero contract as the
/// Node form — see `NodeMetaForm`.
#[derive(Deserialize)]
///
/// No `store_interval` any more (storage_intervals.md §6, S4): Beacon history
/// is the Hub's storage window. A stale form that still posts the field is
/// accepted and the value ignored — serde skips keys the struct does not name.
struct BeaconMetaForm {
    /// Absent = unchanged: the metadata card posts the description, the
    /// offline card the override (metadata.cs.md §4).
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    offline_k: Option<String>,
    /// The Portal's own name and location (decision #41): stored here and shown at once on
    /// every page. Absent = unchanged; blank hands the text back to the Hub's.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    location: Option<String>,
    /// The tab the redirect returns to: `overview` (Device information) or,
    /// by default, `settings` (the offline card).
    #[serde(default)]
    tab: Option<String>,
}

async fn set_beacon_meta(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(mac): Path<String>,
    Form(form): Form<BeaconMetaForm>,
) -> Response {
    let mac = mac.trim().to_uppercase();
    // Back to the Beacon's own page, on the tab the form lives on — its
    // flash query is read by `beacon_detail_page`.
    let back = match form.tab.as_deref() {
        Some("overview") => "overview",
        _ => "settings",
    };
    let detail = format!("/beacons/{mac}/{back}");
    if state.store.snapshot().devices.is_empty() {
        return Redirect::to(&format!("{detail}?error=no_device")).into_response();
    }
    // 404 for a MAC the store does not know (§19, F7). Without this the
    // route happily writes metadata for a device that does not exist —
    // which is how a removed Beacon's row came back: the operator's browser
    // still had the form open, and the write recreated it. The lookup is
    // also what says which Hub's row is written (devices.hub.md §12 E).
    let Some(hostname) = hostname_for_known_beacon(&state, &mac) else {
        return (StatusCode::NOT_FOUND, "unknown beacon").into_response();
    };

    let offline_k = match form.offline_k.as_deref() {
        None => None,
        Some(raw) => match super::parse_optional_count(raw, "offline_k") {
            Ok(v) => Some(v),
            Err(_) => {
                return Redirect::to(&format!("{detail}?error=invalid_offline_k")).into_response()
            }
        },
    };
    if form
        .description
        .as_deref()
        .is_some_and(|d| d.trim().chars().count() > super::DESCRIPTION_MAX)
    {
        return Redirect::to(&format!("{detail}?error=description_too_long")).into_response();
    }
    if form
        .name
        .as_deref()
        .is_some_and(|n| n.trim().chars().count() > super::NAME_MAX)
    {
        return Redirect::to(&format!("{detail}?error=name_too_long")).into_response();
    }
    let description = form.description.clone().map(super::non_empty);
    let name = form
        .name
        .clone()
        .map(|n| super::non_empty(n.trim().to_string()));
    let location = form
        .location
        .clone()
        .map(|l| super::non_empty(l.trim().to_string()));
    let mut labels = None;

    let saved = super::edit_device_meta(
        &state,
        &current_user,
        &hostname,
        THRESHOLD_SCOPE_EYE,
        &mac,
        |row| {
            if let Some(d) = &description {
                row.description = d.clone();
            }
            if let Some(n) = &name {
                row.name = n.clone();
            }
            if let Some(l) = &location {
                row.location = l.clone();
            }
            labels = Some((row.name.clone(), row.location.clone()));
            if let Some(k) = offline_k {
                row.offline_k_override = k;
            }
        },
    )
    .await;

    // Store first, then show at once (decision #41). Neither Hub keeps a writable name or
    // location for a Node or Beacon, so there is no device copy to relay.
    if saved.is_ok() && (name.is_some() || location.is_some()) {
        if let Some((n, l)) = labels {
            state
                .store
                .set_meta_label(&hostname, THRESHOLD_SCOPE_EYE, &mac, n, l);
        }
    }
    if let Err(err) = &saved {
        tracing::warn!(%err, mac, "failed to save beacon metadata");
    }

    super::audit_action(
        &state,
        &current_user,
        "beacon_meta_set",
        &mac,
        json!({
            "description": description,
            "offline_k_override": offline_k,
            "name": name,
            "location": location,
        }),
        saved.is_ok(),
    )
    .await;

    if saved.is_ok() {
        Redirect::to(&format!("{detail}?ok=meta")).into_response()
    } else {
        Redirect::to(&format!("{detail}?error=save_failed")).into_response()
    }
}

// ---------------------------------------------------------------------
// POST /beacons/{mac}/reconfigure — devices.beacon.md §3.5 (B6, WP6.3)
// ---------------------------------------------------------------------

/// "Odeslat žádost o rekonfiguraci" — the **one** action on the read-only
/// BLE half of the configuration section (§3.5), and the same route behind
/// "Zkusit znovu" on a `failed` Beacon.
///
/// One route, two labels, because it is one act: the Hub rewrites its YAML
/// profile into the Beacon over the GATT configuration service. There is no
/// per-parameter form and no pending flow — nobody edits these values
/// (decision 2026-09-01) — so the whole interaction is a button and
/// `#command-status`.
///
/// Hidden for a `read_only` Beacon rather than offered and rejected — and,
/// since the B6 review (item 4), **refused here as well**. The template not
/// drawing a button is not an access rule: the route is reachable by hand,
/// by a stale tab whose Beacon has since gone read-only, and by anything
/// that keeps a URL. The Hub answers such a write with an error anyway
/// (`beacon-provision` on a `read_only` tag is refused), so the only thing
/// the round trip adds is a failed audit row for a request the portal
/// already knew the answer to.
async fn reconfigure_beacon(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(mac): Path<String>,
) -> Response {
    let mac = mac.trim().to_uppercase();
    const COMMAND: &str = "beacon-provision";
    let cmd = Cmd::new(&state, &current_user, COMMAND);

    // Decision #42: a GATT write goes to the Beacon's home Hub, the only one that dials it.
    let Some(hostname) = gatt_hostname_for_known_beacon(&state, &mac) else {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    };
    if refuses_configuration(&state, &hostname, &mac) {
        super::audit_action(
            &state,
            &current_user,
            "beacon_reconfigure",
            &mac,
            json!({ "status": "refused", "reason": "read_only" }),
            false,
        )
        .await;
        return cmd.fail(&tr("monitoring.beacons.err_read_only"));
    }

    cmd.relay(
        &hostname,
        json!({ "mac": mac }),
        None,
        "beacon_reconfigure",
        &mac,
        json!({}),
    )
    .await
}

// ---------------------------------------------------------------------
// POST /beacons/{mac}/history, POST /beacons/{mac}/history/{job_id}/cancel,
// POST /beacons/{mac}/history-auto — plan/portal/beacon_history.md §4, §7.1
// ---------------------------------------------------------------------

/// The History tab's table, the Node's partial addressed by this Beacon's
/// swap id and base path — the same context `sse::handle_beacon_history_nudge`
/// builds.
async fn render_history_partial(
    state: &AppState,
    hostname: &str,
    mac: &str,
    can_edit: bool,
) -> Response {
    let (jobs, gaps) = tokio::join!(
        state.beacon_history.jobs_ctx(&state.store, hostname, mac),
        state.beacon_history.gaps_ctx(hostname, mac),
    );
    render_page(
        state,
        "partials/backfill_jobs.html",
        context! {
            swap_id => beacon_history::swap_id(mac),
            post_url => beacon_history::post_url(mac),
            can_edit => can_edit,
            jobs => jobs,
            gaps => gaps,
        },
    )
}

/// "Stáhnout historii" — ask the Hub to read a window of the Beacon's own
/// archive over GATT (beacon_history.md §2.1, §4.3).
///
/// The job is created `pending` first, so its id rides in the signed
/// request and every progress message the Hub publishes on `eye/history`
/// lands on this row. One job per Beacon at a time (§4.1): a second request
/// is refused with the running one's window. A body with no window at all
/// is the resume request (§2.1): the Hub continues from what it holds.
///
/// Refusals answer the way the Node's backfill form does
/// (`node_routes::backfill_refused`): the table stays, the reason goes to
/// `#command-status`.
async fn request_beacon_history(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(mac): Path<String>,
    headers: axum::http::HeaderMap,
    Form(form): Form<BackfillForm>,
) -> Response {
    let mac = mac.trim().to_uppercase();
    let htmx = headers.contains_key("hx-request");
    let cmd = Cmd::new(&state, &current_user, beacon_history::COMMAND).on(&mac);
    let refuse = |status: StatusCode, plain: String, message: String| {
        backfill_refused(&state, &current_user, &mac, htmx, status, plain, &message)
    };

    // The job is keyed by the registry; the GATT readout is the home Hub's (decision #42).
    let Some(hostname) = hostname_for_known_beacon(&state, &mac) else {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    };
    let gatt_host =
        gatt_hostname_for_known_beacon(&state, &mac).unwrap_or_else(|| hostname.clone());
    // Decision #18, as for the Node: nothing is asked of a Hub that is not paired.
    if !state.backfill.hub_paired(&gatt_host).await {
        return refuse(
            StatusCode::CONFLICT,
            "hub not paired".to_string(),
            tr("monitoring.backfill.err_hub_not_paired"),
        );
    }
    let (from, to) = if form.is_empty() {
        (None, None)
    } else {
        match form.window(render::now_unix(), &crate::display_tz::current()) {
            Ok((from, to)) => (Some(from), Some(to)),
            Err(plain) => {
                return refuse(
                    StatusCode::BAD_REQUEST,
                    plain.to_string(),
                    tr("monitoring.backfill.err_bad_date"),
                )
            }
        }
    };

    let job = match state
        .beacon_history
        .create(
            &hostname,
            &mac,
            from,
            to,
            beacon_history::MANUAL_TRIGGER,
            Some(current_user.username.clone()),
        )
        .await
    {
        Ok(job) => job,
        Err(BeaconHistoryError::InFlight(running)) => {
            let at = |ts: Option<i64>| {
                ts.map(crate::display_tz::format_minutes)
                    .unwrap_or_else(|| "…".to_string())
            };
            return refuse(
                StatusCode::CONFLICT,
                format!("job {} in flight", running.id),
                tr("monitoring.beacon_history.err_in_flight")
                    .replace("{from}", &at(running.from_ts))
                    .replace("{to}", &at(running.to_ts)),
            );
        }
        Err(err @ BeaconHistoryError::EmptyWindow) => {
            return refuse(
                StatusCode::UNPROCESSABLE_ENTITY,
                err.to_string(),
                tr("monitoring.backfill.err_empty_window"),
            )
        }
        Err(err @ BeaconHistoryError::FutureWindow) => {
            return refuse(
                StatusCode::UNPROCESSABLE_ENTITY,
                err.to_string(),
                tr("monitoring.backfill.err_future_window"),
            )
        }
        Err(BeaconHistoryError::Storage(err)) => {
            tracing::error!(%err, hostname, mac, "beacon history request storage failure");
            return (StatusCode::INTERNAL_SERVER_ERROR, "request failed").into_response();
        }
    };

    match cmd
        .execute(&gatt_host, BeaconHistoryService::params(&job), None)
        .await
    {
        Ok(outcome) => {
            state.beacon_history.settle(&job, outcome.status).await;
            cmd.audit(
                "beacon_history_request",
                &mac,
                json!({ "job": job.id, "from": from, "to": to, "preset": form.preset }),
                &outcome,
            )
            .await;
            if htmx {
                render_history_partial(
                    &state,
                    &hostname,
                    &mac,
                    render::can_edit_thresholds(&current_user),
                )
                .await
            } else {
                cmd.done(outcome)
            }
        }
        // Refused before anything was signed: the job never existed for the Hub.
        Err(refused) => {
            state.beacon_history.discard(&job).await;
            if htmx {
                refuse(
                    StatusCode::CONFLICT,
                    String::new(),
                    refused.message.clone().unwrap_or_default(),
                )
            } else {
                cmd.done(refused)
            }
        }
    }
}

/// Cancels a job the Hub has not picked up yet (`pending`, §4.3). One the
/// Hub acknowledged runs to its end: the readout is the Hub's (§2.4), and
/// there is no wire verb to stop it.
async fn cancel_beacon_history(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path((mac, job_id)): Path<(String, String)>,
) -> Response {
    let mac = mac.trim().to_uppercase();
    let Some(hostname) = hostname_for_known_beacon(&state, &mac) else {
        return (StatusCode::NOT_FOUND, "no device").into_response();
    };
    let cancelled = state.beacon_history.cancel(&hostname, &mac, &job_id).await;
    if let Err(err) = &cancelled {
        tracing::warn!(%err, job_id, mac, "beacon history cancel failed");
    }
    super::audit_action(
        &state,
        &current_user,
        "beacon_history_cancel",
        &mac,
        json!({ "job": job_id }),
        matches!(cancelled, Ok(true)),
    )
    .await;
    render_history_partial(
        &state,
        &hostname,
        &mac,
        render::can_edit_thresholds(&current_user),
    )
    .await
}

#[derive(Deserialize)]
struct HistoryAutoForm {
    /// The checkbox: present when checked, absent when not.
    enabled: Option<String>,
}

/// The automatic download after a return (§4.2), the Node's switch: saved
/// at once into `device_meta.backfill_auto`, scope `eye`. Nothing goes to
/// the Beacon — whether it records is provisioning's (§3).
async fn set_beacon_history_auto(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(mac): Path<String>,
    Form(form): Form<HistoryAutoForm>,
) -> Response {
    let mac = mac.trim().to_uppercase();
    let cmd = Cmd::new(&state, &current_user, beacon_history::COMMAND);
    let Some(hostname) = hostname_for_known_beacon(&state, &mac) else {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    };
    let enabled = form.enabled.is_some();
    let saved = super::edit_device_meta(
        &state,
        &current_user,
        &hostname,
        THRESHOLD_SCOPE_EYE,
        &mac,
        |row| row.backfill_auto = enabled,
    )
    .await;
    super::audit_action(
        &state,
        &current_user,
        "beacon_history_auto_set",
        &mac,
        json!({ "enabled": enabled }),
        saved.is_ok(),
    )
    .await;
    if let Err(err) = &saved {
        tracing::warn!(%err, mac, "failed to save the beacon history switch");
        return cmd.fail(&tr("monitoring.meta.err_save_failed"));
    }
    cmd.ok(&tr(if enabled {
        "monitoring.history_switch.saved_on"
    } else {
        "monitoring.history_switch.saved_off"
    }))
}

// ---------------------------------------------------------------------
// POST /beacons/{mac}/device-config
// ---------------------------------------------------------------------

/// The Device configuration card's check (ui_components.md §6). Absent = unchanged.
#[derive(Deserialize)]
struct BeaconDeviceConfigForm {
    #[serde(default)]
    offline_k: Option<String>,
    /// `on` / `off`.
    #[serde(default)]
    history: Option<String>,
}

/// Both Beacon fields of the card are portal-side (`device_meta`): saved at once, nothing
/// goes to the Beacon. The report interval is cluster-wide and not posted (#141).
async fn set_beacon_device_config(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(mac): Path<String>,
    Form(form): Form<BeaconDeviceConfigForm>,
) -> Response {
    let mac = mac.trim().to_uppercase();
    let cmd = Cmd::new(&state, &current_user, beacon_history::COMMAND);
    let Some(hostname) = hostname_for_known_beacon(&state, &mac) else {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    };
    let offline_k = match form.offline_k.as_deref() {
        None => None,
        Some(raw) => match super::parse_optional_count(raw, "offline_k") {
            Ok(v) => Some(v),
            Err(_) => return cmd.fail(&tr("monitoring.device_config.err_offline_k")),
        },
    };
    let history = form.history.as_deref().map(|h| h == "on");
    let saved = super::edit_device_meta(
        &state,
        &current_user,
        &hostname,
        THRESHOLD_SCOPE_EYE,
        &mac,
        |row| {
            if let Some(k) = offline_k {
                row.offline_k_override = k;
            }
            if let Some(h) = history {
                row.backfill_auto = h;
            }
        },
    )
    .await;
    super::audit_action(
        &state,
        &current_user,
        "beacon_device_config_set",
        &mac,
        json!({ "offline_k_override": offline_k, "backfill_auto": history }),
        saved.is_ok(),
    )
    .await;
    if let Err(err) = &saved {
        tracing::warn!(%err, mac, "failed to save the beacon device configuration");
        return cmd.fail(&tr("monitoring.meta.err_save_failed"));
    }
    cmd.ok(&tr("monitoring.device_config.saved"))
}

// ---------------------------------------------------------------------
// POST /beacons/{mac}/remove
// ---------------------------------------------------------------------

/// Removing a Beacon is **one command**: `beacon-remove`, and nothing else.
///
/// A broadcast has no handshake, so removal is a pure delete (§5) — the
/// Beacon keeps advertising and reappears in the discovered list, which is
/// correct and visible behaviour rather than a bug.
///
/// **The pre-step is gone** (Matej, 2026-09-15 — plan/portal/mqtt/design.md
/// §8, handoff.md §1). It was a best-effort `set_eye_profile` in v1 and this
/// tree had renamed it to `beacon-provision`; ble/plan.md §9.8's judgement
/// on it is "never worked in v1". It could not do the thing it was for
/// either: `ProvisionBeacon { mac }` is the whole wire shape, with **no
/// profile parameter**, so "return it to default advertising" was never
/// expressible — what it actually sent re-wrote the Hub's own YAML fleet
/// profile. Etiquette towards the next network (§8 item 5, central.md §5)
/// stays a real want; it needs a wire field first, and §22.8 records the
/// gap. One verb also means one audit row, and no half-done "reconfigured
/// but still registered" state to explain.
///
/// **Typed confirmation, checked here** (2026-09-15, unifying removal with
/// `remove_node`). Until now this was a one-click button on the list row
/// behind an `hx-confirm`; a row that SSE re-renders every few seconds and
/// reorders under the cursor is the wrong place for a delete, and a native
/// `confirm()` is the dialog people dismiss without reading. The action
/// now lives on the Beacon's own page and the typed MAC is verified on the
/// server — a mismatch is audited as a refusal, because "someone tried to
/// remove this and got the identifier wrong" is a thing a trail should say.
///
/// A full-page POST + redirect, like `remove_node`: the row is gone
/// afterwards, so there is nothing to swap back into the page.
///
/// `beacon-remove` is `destructive` (design.md §4.2), so this route is
/// `RequireAdmin` (§13 item 3, ratified 2026-09-15): a manager is refused at
/// the gate, before anything is signed or published.
async fn remove_beacon(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(mac): Path<String>,
    Form(form): Form<RemoveBeaconForm>,
) -> Response {
    let mac = mac.to_uppercase();
    const COMMAND: &str = "beacon-remove";
    let cmd = Cmd::new(&state, &current_user, COMMAND);
    let detail = format!("/beacons/{mac}/overview");

    let Some(hostname) = hostname_for_known_beacon(&state, &mac) else {
        return Redirect::to("/beacons?error=no_device").into_response();
    };

    if form.confirm.trim().to_uppercase() != mac {
        super::audit_action(
            &state,
            &current_user,
            "beacon_remove",
            &mac,
            json!({ "refused": "confirmation_mismatch" }),
            false,
        )
        .await;
        return Redirect::to(&format!("{detail}?error=confirm_mismatch")).into_response();
    }

    // A refusal before signing says why (an unpaired Hub, #109) rather than "could not be
    // removed"; not audited, nothing was sent.
    let outcome = match cmd
        .execute_or_code(&hostname, json!({ "mac": mac }), None)
        .await
    {
        Ok(outcome) => outcome,
        Err(code) => return Redirect::to(&format!("{detail}?error={code}")).into_response(),
    };

    // `device_meta` goes with the device (§19, F7) — see
    // `routes::forget_device_meta` for why, and for the B4 hook.
    if outcome.status == "success" {
        state.store.forget_registered_beacon(&mac);
        super::forget_device_meta(&state, &hostname, THRESHOLD_SCOPE_EYE, &mac).await;
        // The bounds go too — the dialog says so, and a MAC is reused.
        super::forget_field_thresholds(&state, &hostname, THRESHOLD_SCOPE_EYE, &mac).await;
        // Its open alarms end: nothing will ever clear them (`device_removed`).
        state
            .alarms
            .close_removed_device(&hostname, crate::alarms::AlarmTarget::Beacon(mac.clone()))
            .await;
        // Its calibration reminder goes too; the certificates stay
        // (calibration.cs.md §4).
        state
            .calibration
            .forget_device(&crate::calibration::DeviceKey::new(
                &hostname,
                crate::calibration::CalibrationScope::Beacon,
                &mac,
            ))
            .await;
    }

    super::audit_action(
        &state,
        &current_user,
        "beacon_remove",
        &mac,
        json!({ "status": outcome.status }),
        outcome.status == "success",
    )
    .await;

    if outcome.status == "success" {
        // Back to the list: the page the operator came from no longer has
        // a row for this Beacon, and staying on a detail page for a
        // registration that is gone would 404 on the next refresh.
        Redirect::to("/beacons?ok=removed").into_response()
    } else {
        Redirect::to(&format!("{detail}?error=remove_failed")).into_response()
    }
}

#[derive(Deserialize)]
struct RemoveBeaconForm {
    /// The typed confirmation, checked on the server against the MAC.
    #[serde(default)]
    confirm: String,
}

// ---------------------------------------------------------------------
// POST /beacons/{mac}/thresholds/{field}
// ---------------------------------------------------------------------

async fn set_beacon_threshold(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path((mac, field)): Path<(String, String)>,
    Form(form): Form<RadioThresholdForm>,
) -> Response {
    let mac = mac.to_uppercase();
    // "Band active" is the portal's own flag — the wire has no such field —
    // so unticking it has to be said in the vocabulary the Hub does have:
    // the band is DELETED there (GitLab #80). Before this it was written to
    // the local row and nothing else, and the Hub went on evaluating the
    // band it had been given, so an unticked band still raised alarms. The
    // bounds stay in the portal's row either way, so ticking it back does
    // not ask the operator to retype them.
    //
    // Same permission and level on both verbs (`set_lorawan_sensor_config`,
    // Manage), so a certificate minted for one covers the other.
    let enabled = form.enabled.is_some();
    let command = match enabled {
        true => "set_eye_field_threshold",
        false => "delete_eye_field_threshold",
    };
    let cmd = Cmd::new(&state, &current_user, command);

    if !fiber_protocol::eye_field_is_thresholdable(&field) {
        return cmd
            .fail(&tr("monitoring.shared.field_not_thresholdable").replace("{field}", &field));
    }

    let (critical_low, warning_low, warning_high, critical_high) = match parse_radio_bounds(&form) {
        Ok(bounds) => bounds,
        Err(msg) => return cmd.fail(&msg),
    };

    let hysteresis = match parse_hysteresis(
        form.hysteresis.as_deref(),
        (critical_low, warning_low, warning_high, critical_high),
    ) {
        Ok(h) => h,
        Err(msg) => return cmd.fail(&msg),
    };

    let Some(hostname) = hostname_for_known_beacon(&state, &mac) else {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    };
    if state
        .store
        .sensor_disabled(&hostname, THRESHOLD_SCOPE_EYE, &mac, &field)
    {
        return cmd.fail(&tr("monitoring.sensor_switch.err_off"));
    }

    // The portal's alone (decision #41), so stored whatever the Hub answers; the command
    // below carries the result for the Hub's own LCD and buzzer (alarm_hysteresis.md §7).
    let _ = save_hysteresis(
        &state,
        &hostname,
        THRESHOLD_SCOPE_EYE,
        &mac,
        &field,
        hysteresis,
    )
    .await;

    let row = NewFieldThreshold {
        hostname: hostname.clone(),
        scope: THRESHOLD_SCOPE_EYE.to_string(),
        entity: mac.clone(),
        field: field.clone(),
        critical_low,
        warning_low,
        warning_high,
        critical_high,
        enabled,
    };

    // A band saved switched off where the Hub holds none — no row, or one that is off, a
    // default band above all (threshold_defaults.md §3) — is the portal's alone: the bounds
    // are kept for the day it is switched on, and nothing is signed or sent. Only a band the
    // Hub is evaluating needs the delete (GitLab #80).
    if !enabled {
        let (h, m, f) = (hostname.clone(), mac.clone(), field.clone());
        let armed = state
            .db
            .run(move |s| s.get_field_threshold(&h, THRESHOLD_SCOPE_EYE, &m, &f))
            .await
            .map(|stored| stored.is_some_and(|t| t.enabled));
        match armed {
            Ok(true) => {}
            Ok(false) => {
                let written = {
                    let row = row.clone();
                    let actor = current_user.username.clone();
                    state
                        .db
                        .write(move |s| s.upsert_field_threshold(&row, Some(&actor)))
                        .await
                };
                if let Err(err) = written {
                    tracing::warn!(%err, mac, field, "a switched-off eye band was not stored");
                    return cmd.fail(&tr("monitoring.pending.err_draft_failed"));
                }
                crate::sync::note_save(
                    &state.db,
                    &state.relay,
                    &hostname,
                    "delete_eye_field_threshold",
                    &json!({ "mac": mac, "field": field }),
                    &current_user.username,
                )
                .await;
                super::audit_action(
                    &state,
                    &current_user,
                    "beacon_threshold_set",
                    &mac,
                    json!({
                        "field": field,
                        "critical_low": critical_low,
                        "warning_low": warning_low,
                        "warning_high": warning_high,
                        "critical_high": critical_high,
                        "enabled": false,
                        "sent": false,
                    }),
                    true,
                )
                .await;
                return cmd.ok(&tr("monitoring.threshold.saved_off"));
            }
            // Unknown whether the Hub holds it: the delete is the safe side.
            Err(err) => {
                tracing::warn!(%err, mac, field, "eye band unreadable; clearing on the Hub")
            }
        }
    }

    // The delete verb takes the identity alone (`control-core`
    // `MqttCommand::DeleteBeaconFieldThreshold`); sending bounds with it
    // would be inventing a parameter the agent does not parse.
    let params = match enabled {
        true => json!({
            "mac": mac,
            "field": field,
            "critical_low": critical_low,
            "warning_low": warning_low,
            "warning_high": warning_high,
            "critical_high": critical_high,
            "hysteresis":
                effective_hysteresis(&state, &hostname, THRESHOLD_SCOPE_EYE, &mac, &field).await,
        }),
        false => json!({ "mac": mac, "field": field }),
    };

    // Decision #41: the Portal's copy is the band, stored before the command and kept
    // whatever the Hub answers; the Hub's copy is secondary.
    mirror_band(&state, row, &current_user.username).await;

    cmd.dispatch_stored(
        &hostname,
        params,
        "beacon_threshold_set",
        &mac,
        json!({
            "field": field,
            "critical_low": critical_low,
            "warning_low": warning_low,
            "warning_high": warning_high,
            "critical_high": critical_high,
            "enabled": enabled,
        }),
    )
    .await
}

// ---------------------------------------------------------------------
// POST /beacons/{mac}/sensors/{field}
// ---------------------------------------------------------------------

/// Switches a Beacon's temperature or humidity off or on (GitLab #120, devices.beacon.md,
/// amendment 2026-10-13). Off deletes the sensor's history (its column in `eye_readings`),
/// alarms and bound history, and its bound: a band the Hub evaluates is deleted there first
/// (`delete_eye_field_threshold`), and a refusal leaves the sensor on. On stores it again from
/// the next reading, with no bound. A Beacon has no alarm table of its own, so nothing is
/// drafted.
async fn set_beacon_sensor_enabled(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path((mac, field)): Path<(String, String)>,
    Form(form): Form<SensorSwitchForm>,
) -> Response {
    let mac = mac.to_uppercase();
    let enable = form.switch_on();
    let cmd = Cmd::new(&state, &current_user, "delete_eye_field_threshold");

    if state.store.snapshot().devices.is_empty() {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    }
    let Some(def) = fiber_protocol::eye_field(&field)
        .filter(|_| matches!(field.as_str(), "temperature" | "humidity"))
    else {
        return cmd.fail(&tr("monitoring.shared.unknown_field").replace("{field}", &field));
    };
    // A Beacon two Hubs both hear is one Beacon on the page, and `hub_of_beacon` shows
    // whichever heard it last. The switch therefore acts on every Hub that reports it:
    // switched off on one only, the sensor would be back on the page, with its history,
    // as soon as the other Hub is the fresher one.
    let hostnames = hostnames_reporting_beacon(&state, &mac);
    if hostnames.is_empty() {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    }
    let shown = crate::i18n::field_label(crate::i18n::current_lang(), &field, def.label);
    let redirect = format!("/beacons/{mac}/overview");

    if enable {
        // On also reaches a Hub that switched it off but has not reported the Beacon since
        // (a portal restart): left off there, it would come back off once that Hub reports.
        let mut hostnames = hostnames;
        let (m, f) = (mac.clone(), field.clone());
        if let Ok(stored) = state.db.run(|s| s.list_disabled_sensors()).await {
            for row in stored {
                if row.scope == THRESHOLD_SCOPE_EYE
                    && row.entity == m
                    && row.field == f
                    && !hostnames.contains(&row.hostname)
                {
                    hostnames.push(row.hostname);
                }
            }
        }
        let mut switched = false;
        for hostname in &hostnames {
            let (h, m, f) = (hostname.clone(), mac.clone(), field.clone());
            let Ok(on) = state
                .db
                .write(move |s| s.enable_sensor(&h, THRESHOLD_SCOPE_EYE, &m, &f))
                .await
            else {
                return cmd.fail(&tr("monitoring.sensor_switch.err_failed"));
            };
            switched |= on;
            state
                .store
                .set_sensor_disabled(hostname, THRESHOLD_SCOPE_EYE, &mac, &field, false);
        }
        if !switched {
            return cmd.ok(&tr("monitoring.sensor_switch.already_on").replace("{sensor}", &shown));
        }
        super::audit_action(
            &state,
            &current_user,
            "sensor_enable",
            &mac,
            json!({ "field": field, "hubs": hostnames }),
            true,
        )
        .await;
        return sensor_switch_redirect(&format!("{redirect}?ok=sensor_on.{field}"));
    }

    // The bounds first, on every Hub, before anything here goes. One a Hub evaluates is
    // deleted there: the Hub would go on raising alarms for a sensor the portal no longer
    // shows (GitLab #80).
    let mut sent = false;
    for hostname in &hostnames {
        let stored = {
            let (h, m, f) = (hostname.clone(), mac.clone(), field.clone());
            state
                .db
                .run(move |s| s.get_field_threshold(&h, THRESHOLD_SCOPE_EYE, &m, &f))
                .await
        };
        // Unknown whether the Hub holds it: the delete is the safe side.
        let armed = stored
            .as_ref()
            .map_or(true, |t| t.as_ref().is_some_and(|t| t.enabled));
        if armed {
            let outcome = match cmd
                .execute(hostname, json!({ "mac": mac, "field": field }), None)
                .await
            {
                Ok(outcome) => outcome,
                Err(refused) => return cmd.done(refused),
            };
            if !super::command::succeeded(&outcome) {
                let message = outcome.message.clone().unwrap_or_default();
                return cmd
                    .fail(&tr("monitoring.sensor_switch.err_hub").replace("{message}", &message));
            }
            sent = true;
        }
        if armed || stored.is_ok_and(|t| t.is_some()) {
            let (h, m, f) = (hostname.clone(), mac.clone(), field.clone());
            if let Err(err) = state
                .db
                .write(move |s| s.delete_field_threshold(&h, THRESHOLD_SCOPE_EYE, &m, &f))
                .await
            {
                tracing::warn!(%err, hostname, mac, field, "switched-off sensor kept its stored band");
            }
            state.alarms.request_orphan_sweep();
        }
    }

    let mut purged: Option<portal_storage::SensorPurge> = None;
    for hostname in &hostnames {
        let purge = {
            let (h, m, f) = (hostname.clone(), mac.clone(), field.clone());
            let sensor_id = format!("eye:{mac}:{}", def.label);
            let actor = current_user.username.clone();
            state
                .db
                .write(move |s| {
                    s.disable_sensor(&h, THRESHOLD_SCOPE_EYE, &m, &f, &sensor_id, &actor)
                })
                .await
        };
        let purge = match purge {
            Ok(purge) => purge,
            Err(err) => {
                tracing::warn!(%err, hostname, mac, field, "sensor not switched off");
                return cmd.fail(&tr("monitoring.sensor_switch.err_failed"));
            }
        };
        state
            .store
            .set_sensor_disabled(hostname, THRESHOLD_SCOPE_EYE, &mac, &field, true);
        if let Some(purge) = purge {
            let total = purged.get_or_insert_with(Default::default);
            total.readings += purge.readings;
            total.alarms += purge.alarms;
            total.threshold_history += purge.threshold_history;
            total.group_members += purge.group_members;
        }
    }
    let Some(purge) = purged else {
        return cmd.ok(&tr("monitoring.sensor_switch.already_off").replace("{sensor}", &shown));
    };
    super::audit_action(
        &state,
        &current_user,
        "sensor_disable",
        &mac,
        json!({
            "field": field,
            "hubs": hostnames,
            "readings": purge.readings,
            "alarms": purge.alarms,
            "threshold_history": purge.threshold_history,
            "group_members": purge.group_members,
            "hub_band_deleted": sent,
        }),
        true,
    )
    .await;
    sensor_switch_redirect(&format!("{redirect}?ok=sensor_off.{field}"))
}

/// Writes a Beacon band into the portal's copy (`field_thresholds`), which
/// the Beacon alarm engine grades against. A failed write is logged: the
/// device already has the band, and the page reads the portal's copy.
pub(crate) async fn mirror_band(state: &AppState, row: NewFieldThreshold, actor: &str) {
    let actor = actor.to_string();
    let (mac, field) = (row.entity.clone(), row.field.clone());
    if let Err(err) = state
        .db
        .write(move |s| s.upsert_field_threshold(&row, Some(&actor)))
        .await
    {
        tracing::warn!(%err, mac, field, "local eye field_threshold upsert failed");
    }
    // A band switched off or emptied ends its open alarm (`rule_removed`).
    state.alarms.request_orphan_sweep();
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn valid_macs_accepted() {
        assert!(is_valid_mac("AA:BB:CC:DD:EE:FF"));
        assert!(is_valid_mac("00:11:22:33:44:55"));
    }

    #[test]
    fn invalid_macs_rejected() {
        assert!(!is_valid_mac("AA:BB:CC:DD:EE"));
        assert!(!is_valid_mac("AABBCCDDEEFF"));
        assert!(!is_valid_mac("ZZ:BB:CC:DD:EE:FF"));
        assert!(!is_valid_mac("AA-BB-CC-DD-EE-FF"));
    }

    #[test]
    fn err_message_has_a_fallback() {
        assert!(beacon_err_message("something-nobody-sends").is_some());
    }

    /// `integration::eye_tag`, for test modules outside this file.
    pub(crate) fn test_eye_tag(mac: &str) -> fiber_protocol::payloads::eye::EyeTag {
        integration::eye_tag(mac)
    }

    mod integration {
        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use tower::ServiceExt;

        use crate::auth::{Role, SESSION_COOKIE};
        use crate::routes::test_support::{login_as_role, test_state};
        use crate::state::events::IngestEvent;
        use fiber_protocol::payloads::status::DeviceStatus;

        /// Puts one Beacon into the store, which since §19 (F7) is what
        /// makes it addressable: a `device_meta` write for a MAC the store
        /// does not know is a 404, not a row.
        /// An `eye/sensors` tag with nothing set — the base every test
        /// here spreads its own fields over (`..eye_tag(mac)`).
        pub(super) fn eye_tag(mac: &str) -> fiber_protocol::payloads::eye::EyeTag {
            fiber_protocol::payloads::eye::EyeTag {
                gateway: None,
                mac: mac.into(),
                name: None,
                temperature_c: None,
                humidity_pct: None,
                battery_mv: None,
                low_battery: None,
                magnet_present: None,
                magnet_detected: None,
                moving: None,
                movement_count: None,
                pitch_deg: None,
                roll_deg: None,
                rssi: None,
                last_seen_ts: None,
                stale: None,
                provisioning: None,
                discovered: None,
                field_alarm_states: std::collections::HashMap::new(),
                alarm_state: None,
                ble_config: None,
            }
        }

        pub(super) fn seed_beacon(state: &crate::http::AppState, mac: &str) {
            state.store.apply(IngestEvent::EyeSensors {
                hostname: "h1".into(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: false,
                    tags: vec![eye_tag(mac)],
                },
            });
        }

        async fn post(
            router: &axum::Router,
            path: &str,
            token: &str,
            body: &str,
        ) -> axum::response::Response {
            router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .expect("response")
        }

        /// The Beacon override round-trips through the route and is audited.
        /// The MAC is uppercased into the row key the same way `add_beacon`
        /// uppercases it, so the two paths address one row — and the
        /// redirect lands back on the page that carries the form (B5), which
        /// is the only surface that renders the `?ok=` flash: until B5 it
        /// went to `/beacons`, where a successful save said nothing at all.
        #[tokio::test]
        async fn a_manager_writing_beacon_metadata_lands_back_on_the_detail_page() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "h1".into(),
                status: DeviceStatus::Online,
            });
            seed_beacon(&state, "AA:BB:CC:DD:EE:01");
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                "/beacons/aa:bb:cc:dd:ee:01/meta",
                &manager,
                "description=freezer&offline_k=&store_interval=30",
            )
            .await;
            // A stale page that still posts `store_interval` saves fine; the
            // field is gone (storage_intervals.md S4) and its value ignored.
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            assert_eq!(
                response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/beacons/AA:BB:CC:DD:EE:01/settings?ok=meta")
            );

            let row = ({
                let a0 = "h1".to_string();
                let a1 = portal_storage::THRESHOLD_SCOPE_EYE.to_string();
                let a2 = "AA:BB:CC:DD:EE:01".to_string();
                state.db.run(move |s| s.get_device_meta(&a0, &a1, &a2))
            })
            .await
            .expect("device_meta")
            .expect("row written");
            assert_eq!(row.description.as_deref(), Some("freezer"));
            assert_eq!(
                row.store_interval_override, None,
                "the retired override is never written"
            );
            assert_eq!(row.offline_k_override, None, "blank means inherit");

            let (rows, _total) = state
                .auth
                .audit_page(portal_storage::AuditFilter {
                    event_type: Some("beacon_meta_set".into()),
                    username: None,
                    since: None,
                    until: None,
                    success: None,
                    search: None,
                    limit: 10,
                    offset: 0,
                    ..Default::default()
                })
                .await
                .expect("audit page");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].target.as_deref(), Some("AA:BB:CC:DD:EE:01"));
        }

        /// Each form edits what it shows and inherits the rest. The repo's
        /// upsert writes every mutable column from `excluded`, so a form
        /// that did not read the row first would null the column it does not
        /// render — here, the Beacon override sitting on a row the Node form
        /// then saves over.
        #[tokio::test]
        async fn a_save_does_not_null_the_column_its_form_does_not_show() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "h1".into(),
                status: DeviceStatus::Online,
            });
            seed_beacon(&state, "AA:BB:CC:DD:EE:01");
            let router = super::super::router().with_state(state.clone());

            post(
                &router,
                "/beacons/AA:BB:CC:DD:EE:01/meta",
                &manager,
                "description=freezer&offline_k=2",
            )
            .await;
            // A second save writes the description and the offline override
            // the form does carry as submitted.
            post(
                &router,
                "/beacons/AA:BB:CC:DD:EE:01/meta",
                &manager,
                "description=freezer&offline_k=2",
            )
            .await;

            let row = ({
                let a0 = "h1".to_string();
                let a1 = portal_storage::THRESHOLD_SCOPE_EYE.to_string();
                let a2 = "AA:BB:CC:DD:EE:01".to_string();
                state.db.run(move |s| s.get_device_meta(&a0, &a1, &a2))
            })
            .await
            .expect("device_meta")
            .expect("row written");
            assert_eq!(row.offline_k_override, Some(2));
            // `backfill_auto` is the Node column this form never renders. It
            // must still be at its schema default rather than having been
            // written to whatever a fresh struct happened to carry.
            assert!(row.backfill_auto);
        }
        // ---- §19 (F10): registration is manager work, not admin ----------

        /// devices.beacon.md §7 (line 342) reads
        /// `| Registrace Beaconu (§4) / odebrání (§5) | **manager** | ano |`,
        /// and §4/§5 repeat "Manažer a vyšší + audit". B0 shipped both as
        /// `RequireAdmin`, so a manager — the role the spec designates for
        /// fleet work — had to find an admin for a routine registration.
        /// Registration is that row's half that still holds.
        ///
        /// **Removal no longer does.** `beacon-remove` is `destructive`
        /// (plan/portal/mqtt/design.md §4.2), ratified with the route move
        /// on 2026-09-15 (§13 item 3), so §7's row is half-overtaken and
        /// `a_manager_is_refused_a_beacon_removal` below pins the other
        /// half. The Czech spec row is owed a dated note.
        ///
        /// No Hub is in the store, so the handler takes its `no_device_yet`
        /// early return: the gate is what is under test, and this keeps the
        /// harness relay (which never resolves) out of it.
        #[tokio::test]
        async fn a_manager_is_past_the_gate_on_beacon_add() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());

            let added = post(
                &router,
                "/beacons/add",
                &manager,
                "mac=AA:BB:CC:DD:EE:01&name=x",
            )
            .await;
            assert_ne!(added.status(), StatusCode::FORBIDDEN);
        }

        /// The bottom of the gate is unchanged whichever role holds the
        /// top: a viewer is refused both, and leaves nothing behind.
        #[tokio::test]
        async fn a_viewer_is_still_refused_beacon_add_and_remove() {
            let (state, _dir) = test_state().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            let router = super::super::router().with_state(state.clone());

            let added = post(
                &router,
                "/beacons/add",
                &viewer,
                "mac=AA:BB:CC:DD:EE:01&name=x",
            )
            .await;
            assert_eq!(added.status(), StatusCode::FORBIDDEN);
            let removed = post(
                &router,
                "/beacons/AA:BB:CC:DD:EE:01/remove",
                &viewer,
                "confirm=AA:BB:CC:DD:EE:01",
            )
            .await;
            assert_eq!(removed.status(), StatusCode::FORBIDDEN);

            let (_, total) = state
                .auth
                .audit_page(portal_storage::AuditFilter {
                    event_type: Some("beacon_remove".to_string()),
                    ..Default::default()
                })
                .await
                .expect("audit page");
            assert_eq!(total, 0, "a refusal never reaches the handler");
        }

        // ---- §19 (F7): device_meta is deleted with its device ------------

        /// Register a MAC with an override, remove it, register the same MAC
        /// for a different physical Beacon: the new device must not inherit
        /// the description, and must not inherit an `offline_k_override`
        /// that would put it on a liveness cadence nobody chose for it.
        ///
        /// Driven through `forget_device_meta` rather than through the route,
        /// because the harness relay never resolves and `remove_beacon` only
        /// reaches this on a `success` outcome.
        #[tokio::test]
        async fn a_re_registered_mac_does_not_inherit_the_last_devices_metadata() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "h1".into(),
                status: DeviceStatus::Online,
            });
            seed_beacon(&state, "AA:BB:CC:DD:EE:01");
            let router = super::super::router().with_state(state.clone());

            post(
                &router,
                "/beacons/AA:BB:CC:DD:EE:01/meta",
                &manager,
                "description=freezer&offline_k=2",
            )
            .await;

            crate::routes::forget_device_meta(
                &state,
                "h1",
                portal_storage::THRESHOLD_SCOPE_EYE,
                "AA:BB:CC:DD:EE:01",
            )
            .await;

            let row = ({
                let a0 = "h1".to_string();
                let a1 = portal_storage::THRESHOLD_SCOPE_EYE.to_string();
                let a2 = "AA:BB:CC:DD:EE:01".to_string();
                state.db.run(move |s| s.get_device_meta(&a0, &a1, &a2))
            })
            .await
            .expect("device_meta");
            assert!(
                row.is_none(),
                "the next Beacon at this address starts from the YAML defaults"
            );
        }

        /// A meta POST for a MAC the store has never seen is a 404 and
        /// writes nothing — otherwise the route plants a row for a device
        /// that does not exist, which is how a removed Beacon's overrides
        /// came back from a stale browser tab.
        #[tokio::test]
        async fn a_meta_post_for_an_unknown_beacon_is_404_and_writes_nothing() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "h1".into(),
                status: DeviceStatus::Online,
            });
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                "/beacons/AA:BB:CC:DD:EE:99/meta",
                &manager,
                "description=ghost&offline_k=",
            )
            .await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);

            let row = ({
                let a0 = "h1".to_string();
                let a1 = portal_storage::THRESHOLD_SCOPE_EYE.to_string();
                let a2 = "AA:BB:CC:DD:EE:99".to_string();
                state.db.run(move |s| s.get_device_meta(&a0, &a1, &a2))
            })
            .await
            .expect("device_meta");
            assert!(row.is_none());
        }

        // ---- B5 (§8 WP5.2/WP5.4): the detail page ------------------------

        async fn get(router: &axum::Router, path: &str, token: &str) -> axum::response::Response {
            router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(path)
                        .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .expect("response")
        }

        async fn body_string(response: axum::response::Response) -> String {
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            String::from_utf8(bytes.to_vec()).expect("utf8")
        }

        /// The page devices.beacon.md §3 asks for, which did not exist
        /// before B5. Both halves of §3.5's configuration split have to be
        /// on it, and the MAC has to be in the "device description" block
        /// rather than only in the header (§3.1).
        #[tokio::test]
        async fn a_manager_sees_both_configuration_halves_on_the_detail_page() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "h1".into(),
                status: DeviceStatus::Online,
            });
            seed_beacon(&state, "AA:BB:CC:DD:EE:01");
            let router = super::super::router().with_state(state.clone());

            let response = get(&router, "/beacons/AA:BB:CC:DD:EE:01/settings", &manager).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = body_string(response).await;

            // minijinja autoescapes `/` in attributes, so URLs come back as
            // `&#x2f;` — unescape once here rather than asserting on that.
            let body = body.replace("&#x2f;", "/");

            assert!(body.contains("AA:BB:CC:DD:EE:01"), "the MAC identifies it");
            // Half 1 — system configuration, editable, immediate: the
            // metadata form pointed at this Beacon's own route.
            assert!(
                body.contains(r#"hx-post="/beacons/AA:BB:CC:DD:EE:01/device-config""#),
                "the manager gets the system-configuration form"
            );
            assert!(
                !body.contains(r#"name="store_interval""#),
                "the store-interval override is gone (storage_intervals.md S4)"
            );
            // Half 2 — BLE parameters, read-only, no values on the wire yet,
            // and since B6 the one action §3.5 allows on it.
            assert!(body.contains("not reported by the Hub yet"));
            assert!(
                body.contains("/beacons/AA:BB:CC:DD:EE:01/reconfigure"),
                "the reconfiguration request is the BLE half's only control"
            );
            // §3.4's thresholds table, on the two fields a Beacon has —
            // Overview's half of the page.
            let overview =
                body_string(get(&router, "/beacons/AA:BB:CC:DD:EE:01/overview", &manager).await)
                    .await
                    .replace("&#x2f;", "/");
            assert!(overview.contains("/beacons/AA:BB:CC:DD:EE:01/thresholds/temperature"));
            assert!(overview.contains("/beacons/AA:BB:CC:DD:EE:01/thresholds/humidity"));
        }

        /// The BLE half shows what the Hub READ BACK once one arrives. A tag
        /// that merely omits the key — a trimmed payload rung — leaves it
        /// standing; an explicit `null` takes it away again.
        #[tokio::test]
        async fn the_ble_half_shows_the_read_back_configuration_until_the_hub_clears_it() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "h1".into(),
                status: DeviceStatus::Online,
            });
            let snapshot = |ble_config| IngestEvent::EyeSensors {
                hostname: "h1".into(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: false,
                    tags: vec![fiber_protocol::payloads::eye::EyeTag {
                        ble_config,
                        ..eye_tag("AA:BB:CC:DD:EE:01")
                    }],
                },
            };
            let verified = fiber_protocol::payloads::eye::EyeBleConfig {
                recording: None,
                logging_interval_min: None,
                advertising_interval_ms: Some(10_000),
                tx_power_dbm: Some(8),
                protocol_type: Some(2),
                protocol: Some("eye_sensor".into()),
                sensors: Some(
                    [
                        ("temperature", true),
                        ("humidity", true),
                        ("magnet", false),
                        ("movement", false),
                    ]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
                ),
                active_sensors: None,
                read_at: Some(1_790_106_435),
                verified: Some(false),
            };
            let router = super::super::router().with_state(state.clone());
            let path = "/beacons/AA:BB:CC:DD:EE:01/settings";

            // The plain read that follows an enrollment.
            state.store.apply(snapshot(Some(Some(verified.clone()))));
            let body = body_string(get(&router, path, &manager).await).await;
            assert!(body.contains("10&nbsp;s"), "the interval, in seconds");
            assert!(body.contains("8&nbsp;dBm"), "the Tx power");
            assert!(
                body.contains("EYE Sensor <span class=\"text-muted-fg tabular-nums\">(2)</span>"),
                "the protocol by name, the raw code beside it"
            );
            let sensor_row = |label: &str, state: &str| {
                let row = body
                    .split("<li class=\"flex gap-2\">")
                    .find(|li| li.contains(&format!(">{label}</span>")))
                    .unwrap_or_else(|| panic!("a sensor row for {label}"));
                row.contains(&format!(">{state}</span>"))
            };
            assert!(sensor_row("Temperature", "on"));
            assert!(sensor_row("Humidity", "on"));
            assert!(
                sensor_row("Magnet", "off"),
                "a sensor the mask leaves off says so"
            );
            assert!(sensor_row("Movement", "off"));
            assert!(body.contains("Read from the beacon"));
            assert!(!body.contains("Verified by reading it back"));

            // A provisioning session that verified what it wrote.
            let verified = fiber_protocol::payloads::eye::EyeBleConfig {
                recording: None,
                logging_interval_min: None,
                verified: Some(true),
                ..verified
            };
            state.store.apply(snapshot(Some(Some(verified))));
            let body = body_string(get(&router, path, &manager).await).await;
            assert!(body.contains("Verified by reading it back after configuring it"));

            state.store.apply(snapshot(None));
            let body = body_string(get(&router, path, &manager).await).await;
            assert!(
                body.contains("10&nbsp;s"),
                "an absent key is no news: the last verified configuration stays"
            );

            state.store.apply(snapshot(Some(None)));
            let body = body_string(get(&router, path, &manager).await).await;
            assert!(!body.contains("10&nbsp;s"), "null clears it");
            assert!(body.contains("not reported by the Hub yet"));
        }

        /// A lowercase MAC in the URL resolves (the store keys uppercase,
        /// same normalisation `set_beacon_meta` does), and a MAC no Hub
        /// knows is a 404 rather than an empty page.
        #[tokio::test]
        async fn the_detail_route_uppercases_the_mac_and_sends_an_unknown_one_to_the_list() {
            let (state, _dir) = test_state().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "h1".into(),
                status: DeviceStatus::Online,
            });
            seed_beacon(&state, "AA:BB:CC:DD:EE:01");
            let router = super::super::router().with_state(state.clone());

            assert_eq!(
                get(&router, "/beacons/aa:bb:cc:dd:ee:01/overview", &viewer)
                    .await
                    .status(),
                StatusCode::OK
            );
            // A removed (or never registered) Beacon: back to the list, and
            // the list says why, instead of a bare "unknown beacon".
            let gone = get(&router, "/beacons/AA:BB:CC:DD:EE:99/overview", &viewer).await;
            assert_eq!(gone.status(), StatusCode::SEE_OTHER);
            assert_eq!(
                gone.headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/beacons?error=unknown_beacon")
            );
            let list =
                body_string(get(&router, "/beacons?error=unknown_beacon", &viewer).await).await;
            assert!(
                list.contains("not registered"),
                "the list explains the redirect"
            );

            // Heard but not registered (a candidate, or a Beacon just removed
            // that keeps advertising): the page to be on is registration,
            // with the MAC and the Hub filled in.
            state.store.apply(IngestEvent::EyeSensors {
                hostname: "h1".into(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: false,
                    tags: vec![fiber_protocol::payloads::eye::EyeTag {
                        discovered: Some(true),
                        ..eye_tag("AA:BB:CC:DD:EE:02")
                    }],
                },
            });
            let candidate = get(&router, "/beacons/AA:BB:CC:DD:EE:02/settings", &viewer).await;
            assert_eq!(candidate.status(), StatusCode::SEE_OTHER);
            assert_eq!(
                candidate
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/beacons/add?mac=AA:BB:CC:DD:EE:02&hub=h1")
            );
        }

        /// A Beacon registered on one Hub and overheard as a stranger by
        /// another opens its detail page, not the registration redirect,
        /// and resolves its owner whichever hostname sorts first.
        #[tokio::test]
        async fn a_stranger_copy_on_another_hub_does_not_shadow_the_owner() {
            let (state, _dir) = test_state().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            for (host, discovered) in [("a-client", Some(true)), ("b-owner", None)] {
                state.store.apply(IngestEvent::Status {
                    hostname: host.into(),
                    status: DeviceStatus::Online,
                });
                state.store.apply(IngestEvent::EyeSensors {
                    hostname: host.into(),
                    payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                        timestamp: None,
                        discovered_capped: false,
                        tags: vec![fiber_protocol::payloads::eye::EyeTag {
                            discovered,
                            ..eye_tag("AA:BB:CC:DD:EE:03")
                        }],
                    },
                });
            }
            assert_eq!(
                state
                    .store
                    .snapshot()
                    .hub_of_beacon("AA:BB:CC:DD:EE:03")
                    .map(|(d, _)| d.hostname.clone())
                    .as_deref(),
                Some("b-owner")
            );
            let router = super::super::router().with_state(state.clone());
            let page = get(&router, "/beacons/AA:BB:CC:DD:EE:03/overview", &viewer).await;
            assert_eq!(page.status(), StatusCode::OK);
        }

        /// UX-033: with no limit set the header badge says so instead of a
        /// green "Normal", as the Monitoring card does.
        #[tokio::test]
        async fn a_beacon_without_limits_is_not_badged_normal() {
            let (state, _dir) = test_state().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "h1".into(),
                status: DeviceStatus::Online,
            });
            state.store.apply(IngestEvent::EyeSensors {
                hostname: "h1".into(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: false,
                    tags: vec![fiber_protocol::payloads::eye::EyeTag {
                        temperature_c: Some(4.0),
                        alarm_state: Some("NORMAL".into()),
                        ..eye_tag("AA:BB:CC:DD:EE:01")
                    }],
                },
            });
            let router = super::super::router().with_state(state.clone());

            let page =
                body_string(get(&router, "/beacons/AA:BB:CC:DD:EE:01/overview", &viewer).await)
                    .await;
            assert!(
                page.contains(r#"badge-status-neutral">no limit set"#),
                "{page}"
            );
            assert!(!page.contains(r#"badge-status-normal">Normal"#), "{page}");
        }

        /// A viewer reads the page but edits nothing. A legacy
        /// `store_interval_override` left in the DB column is not shown:
        /// the Hub's storage window decides history now (S4), not the Portal.
        #[tokio::test]
        async fn a_viewer_reads_the_page_and_sees_no_store_interval() {
            let (state, _dir) = test_state().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "h1".into(),
                status: DeviceStatus::Online,
            });
            seed_beacon(&state, "AA:BB:CC:DD:EE:01");
            ({
                let a0 = portal_storage::NewDeviceMeta {
                    hostname: "h1".into(),
                    scope: portal_storage::THRESHOLD_SCOPE_EYE.into(),
                    entity: "AA:BB:CC:DD:EE:01".into(),
                    name: None,
                    description: Some("mrazak sklad".into()),
                    added_by: None,
                    offline_k_override: None,
                    backfill_auto: true,
                    store_interval_override: Some(5),
                    serial: None,
                    location: None,
                };
                state.db.write(move |s| s.upsert_device_meta(&a0))
            })
            .await
            .expect("seed device_meta");
            let router = super::super::router().with_state(state.clone());

            let body =
                body_string(get(&router, "/beacons/AA:BB:CC:DD:EE:01/settings", &viewer).await)
                    .await
                    .replace("&#x2f;", "/");
            assert!(
                !body.contains(r#"action="/beacons/AA:BB:CC:DD:EE:01/meta""#),
                "a viewer gets no form to post"
            );
            // The description moved to the Overview's Device information
            // block (metadata.cs.md §5): plain text there, no form either.
            let overview =
                body_string(get(&router, "/beacons/AA:BB:CC:DD:EE:01/overview", &viewer).await)
                    .await;
            assert!(
                overview.contains("mrazak sklad"),
                "but does see the description"
            );
            assert!(!overview.contains("data-te-form"), "{overview}");
            assert!(
                !body.contains(">5<"),
                "a legacy store-interval override is not rendered"
            );
        }

        /// §5's Beacon offline alert reaching the page at last. Before B5,
        /// `beacon_ctx` took no liveness input at all, so B2's verdict
        /// existed and was rendered nowhere — the same shape of hole §19.2
        /// found on the Node list.
        #[tokio::test]
        async fn a_silent_beacon_draws_the_fault_triangle_on_its_page() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "h1".into(),
                status: DeviceStatus::Online,
            });
            // A Beacon last heard from a day ago, with an override that
            // makes one minute of silence enough.
            state.store.apply(IngestEvent::EyeSensors {
                hostname: "h1".into(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: false,
                    tags: vec![fiber_protocol::payloads::eye::EyeTag {
                        last_seen_ts: Some(crate::render::now_unix() - 86_400),
                        ..eye_tag("AA:BB:CC:DD:EE:01")
                    }],
                },
            });
            ({
                let a0 = portal_storage::NewDeviceMeta {
                    hostname: "h1".into(),
                    scope: portal_storage::THRESHOLD_SCOPE_EYE.into(),
                    entity: "AA:BB:CC:DD:EE:01".into(),
                    name: None,
                    description: None,
                    added_by: None,
                    offline_k_override: Some(1),
                    backfill_auto: true,
                    store_interval_override: Some(1),
                    serial: None,
                    location: None,
                };
                state.db.write(move |s| s.upsert_device_meta(&a0))
            })
            .await
            .expect("seed device_meta");
            state
                .offline
                .pass(&state.store, crate::render::now_unix())
                .await;

            let router = super::super::router().with_state(state.clone());
            let body =
                body_string(get(&router, "/beacons/AA:BB:CC:DD:EE:01/overview", &manager).await)
                    .await;
            // The icon macro inlines only the SVG path, so the assertion is
            // on the tooltip text the operator actually reads — which is
            // also what proves the dynamic i18n key resolves.
            assert!(
                body.contains("Beacon is not broadcasting"),
                "the offline fault has to be visible, not only computed"
            );
        }

        /// §16.3's N+1, on the half B5 owns. Four sites each awaited one
        /// `list_field_thresholds` per Beacon; `render::BeaconRowData` makes
        /// it two queries for the whole fleet. Timed over the data-access
        /// phase alone and bounded loosely, exactly as the Node twin
        /// (`the_row_data_for_five_hundred_nodes_is_two_queries_not_a_thousand`)
        /// is and for the same reason — see §15.4.
        #[tokio::test]
        async fn the_row_data_for_five_hundred_beacons_is_two_queries_not_a_thousand() {
            let (state, _dir) = test_state().await;
            let tags: Vec<_> = (0..500)
                .map(|i| fiber_protocol::payloads::eye::EyeTag {
                    temperature_c: Some(4.0),
                    ..eye_tag(&format!("AA:BB:CC:DD:{:02X}:{:02X}", i / 256, i % 256))
                })
                .collect();
            state.store.apply(IngestEvent::EyeSensors {
                hostname: "h1".into(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: false,
                    tags,
                },
            });
            let snapshot = state.store.snapshot();
            let device = &snapshot.devices["h1"];
            assert_eq!(
                device.ble_sensors.len(),
                500,
                "the fixture must actually build 500 Beacons"
            );

            let started = std::time::Instant::now();
            for _ in 0..5 {
                let rows = super::super::load_beacon_row_data(&state, "h1").await;
                let built: Vec<_> = device
                    .ble_sensors
                    .values()
                    .map(|b| {
                        crate::render::beacon_ctx(
                            rows.inputs(b, crate::render::beacon_hubs(&snapshot, &b.mac)),
                        )
                    })
                    .collect();
                assert_eq!(built.len(), 500);
            }
            let per_pass = started.elapsed() / 5;

            assert!(
                per_pass < std::time::Duration::from_millis(500),
                "row data for 500 Beacons took {per_pass:?} — §8's gate. The cause is \
                 almost certainly per-row database access; see render::BeaconRowData."
            );
        }

        // ---- B6/WP6.1: the scan page ------------------------------------

        /// One snapshot with one registered Beacon and one stranger.
        fn seed_scan(state: &crate::http::AppState) {
            state.store.apply(IngestEvent::Status {
                hostname: "h1".into(),
                status: DeviceStatus::Online,
            });
            state.store.apply(IngestEvent::EyeSensors {
                hostname: "h1".into(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: false,
                    tags: vec![
                        fiber_protocol::payloads::eye::EyeTag {
                            // `null` — the wire's "registered".
                            discovered: None,
                            ..eye_tag("AA:BB:CC:DD:EE:01")
                        },
                        fiber_protocol::payloads::eye::EyeTag {
                            discovered: Some(true),
                            rssi: Some(-55),
                            ..eye_tag("AA:BB:CC:DD:EE:02")
                        },
                    ],
                },
            });
        }

        /// GitLab #120: on the list, a switched-off sensor has no value line; with both off
        /// the cell reads "vyp" rather than two dashes.
        #[tokio::test]
        async fn the_list_hides_a_switched_off_sensor() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "h1".into(),
                status: DeviceStatus::Online,
            });
            state.store.apply(IngestEvent::EyeSensors {
                hostname: "h1".into(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: false,
                    tags: vec![fiber_protocol::payloads::eye::EyeTag {
                        temperature_c: Some(4.5),
                        humidity_pct: Some(61),
                        ..eye_tag("AA:BB:CC:DD:EE:01")
                    }],
                },
            });
            let router = super::super::router().with_state(state.clone());
            let values = |page: String| {
                let start = page
                    .find(r#"<td data-column="values""#)
                    .expect("values cell");
                let rest = &page[start..];
                rest[..rest.find("</td>").expect("cell end")].to_string()
            };
            let mac = "AA:BB:CC:DD:EE:01";

            let cell = values(body_string(get(&router, "/beacons", &manager).await).await);
            assert!(
                cell.contains("4.5&nbsp;°C") && cell.contains("61&nbsp;%"),
                "{cell}"
            );

            state.store.set_sensor_disabled(
                "h1",
                portal_storage::THRESHOLD_SCOPE_EYE,
                mac,
                "humidity",
                true,
            );
            let cell = values(body_string(get(&router, "/beacons", &manager).await).await);
            assert!(cell.contains("4.5&nbsp;°C"), "{cell}");
            assert!(!cell.contains('%') && !cell.contains("--"), "{cell}");

            state.store.set_sensor_disabled(
                "h1",
                portal_storage::THRESHOLD_SCOPE_EYE,
                mac,
                "temperature",
                true,
            );
            let cell = values(body_string(get(&router, "/beacons", &manager).await).await);
            assert!(!cell.contains("°C") && !cell.contains("--"), "{cell}");
            assert!(
                cell.contains(&crate::routes::tr("monitoring.threshold_table.off_label")),
                "{cell}"
            );
        }

        /// GitLab #120: a Beacon two Hubs both report is switched off on both. The page shows
        /// whichever Hub heard it last, so off on one only came back on with the other.
        #[tokio::test]
        async fn a_sensor_switch_acts_on_every_hub_reporting_the_beacon() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let mac = "AA:BB:CC:DD:EE:02";
            for hostname in ["h1", "h2"] {
                state.store.apply(IngestEvent::Status {
                    hostname: hostname.into(),
                    status: DeviceStatus::Online,
                });
                state.store.apply(IngestEvent::EyeSensors {
                    hostname: hostname.into(),
                    payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                        timestamp: None,
                        discovered_capped: false,
                        tags: vec![eye_tag(mac)],
                    },
                });
            }
            let router = super::super::router().with_state(state.clone());
            let off = |state: &crate::http::AppState| {
                ["h1", "h2"].map(|h| {
                    state.store.sensor_disabled(
                        h,
                        portal_storage::THRESHOLD_SCOPE_EYE,
                        mac,
                        "humidity",
                    )
                })
            };

            let path = format!("/beacons/{mac}/sensors/humidity");
            post(&router, &path, &manager, "enabled=false").await;
            assert_eq!(off(&state), [true, true]);
            let stored = state
                .db
                .run(|s| s.list_disabled_sensors())
                .await
                .expect("read");
            assert_eq!(stored.len(), 2, "{stored:?}");

            // A Hub that switched it off and has not reported the Beacon since.
            state
                .db
                .write(move |s| {
                    s.disable_sensor(
                        "h3",
                        portal_storage::THRESHOLD_SCOPE_EYE,
                        mac,
                        "humidity",
                        "eye:AA:BB:CC:DD:EE:02:Humidity",
                        "manager1",
                    )
                })
                .await
                .expect("switch off on h3");

            post(&router, &path, &manager, "enabled=true").await;
            assert_eq!(off(&state), [false, false]);
            let stored = state
                .db
                .run(|s| s.list_disabled_sensors())
                .await
                .expect("read");
            assert!(stored.is_empty(), "{stored:?}");
        }

        /// devices.beacon.md §4: the scan table is the *unregistered* half.
        /// A registered Beacon on it would read as "not yet added" and
        /// invite a duplicate registration.
        #[tokio::test]
        async fn the_add_page_lists_only_discovered_beacons() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_scan(&state);
            let router = super::super::router().with_state(state.clone());

            let body = body_string(get(&router, "/beacons/add", &manager).await).await;
            assert!(body.contains("AA:BB:CC:DD:EE:02"), "{body}");
            assert!(
                !body.contains("AA:BB:CC:DD:EE:01"),
                "a registered Beacon is not a scan candidate: {body}"
            );
            // ...and the prefill button carries the MAC as data, not as an
            // inline JavaScript literal.
            assert!(body.contains(r#"data-mac="AA:BB:CC:DD:EE:02""#), "{body}");

            // The registered half is still on `/beacons`, and the candidate
            // is not.
            let list = body_string(get(&router, "/beacons", &manager).await).await;
            assert!(list.contains("AA:BB:CC:DD:EE:01"), "{list}");
            assert!(
                !list.contains("AA:BB:CC:DD:EE:02"),
                "candidates moved to /beacons/add in B6: {list}"
            );
        }

        /// UI-12 on the page: a capped snapshot says so rather than passing
        /// for a quiet site.
        #[tokio::test]
        async fn discovered_capped_renders_the_cap_notice() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_scan(&state);
            let router = super::super::router().with_state(state.clone());

            let quiet = body_string(get(&router, "/beacons/add", &manager).await).await;
            assert!(
                !quiet.contains("more beacons than it can report"),
                "{quiet}"
            );

            state.store.apply(IngestEvent::EyeSensors {
                hostname: "h1".into(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: true,
                    tags: vec![fiber_protocol::payloads::eye::EyeTag {
                        discovered: Some(true),
                        rssi: Some(-55),
                        ..eye_tag("AA:BB:CC:DD:EE:02")
                    }],
                },
            });
            let capped = body_string(get(&router, "/beacons/add", &manager).await).await;
            assert!(
                capped.contains("more beacons than it can report"),
                "{capped}"
            );
        }

        // ---- B6/WP6.3–6.6: registration, provisioning, commands --------

        const HOST: &str = "h1";

        /// One audit event type's rows, newest first.
        async fn audit_rows(
            state: &crate::http::AppState,
            event_type: &str,
        ) -> Vec<portal_storage::AuditEntry> {
            state
                .auth
                .audit_page(portal_storage::AuditFilter {
                    event_type: Some(event_type.into()),
                    limit: 50,
                    ..Default::default()
                })
                .await
                .expect("audit page")
                .0
        }

        async fn pair_hub(state: &crate::http::AppState) {
            state
                .keys
                .store_pairing(
                    HOST,
                    Some("test-ca".into()),
                    "00".repeat(32),
                    [7u8; 32],
                    None,
                    None,
                )
                .await
                .expect("store pairing");
        }

        /// Polls the relay's tape for a frame on `suffix` at or after `from`.
        /// Real time on purpose: the handshake crosses a `spawn_blocking`
        /// (the key mint), which a paused clock would run ahead of.
        async fn wait_for(
            state: &crate::http::AppState,
            suffix: &str,
            from: usize,
        ) -> Option<serde_json::Value> {
            for _ in 0..600 {
                if let Some((_, body)) = state
                    .relay
                    .published()
                    .into_iter()
                    .skip(from)
                    .find(|(topic, _)| topic.ends_with(suffix))
                {
                    return Some(body);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            None
        }

        /// Plays the Hub's part of the signed-command handshake `n` times, so
        /// a route test can observe what *success* does.
        ///
        /// `test_state`'s relay has no `run` loop and nothing answering it, so
        /// every signed command it issues would otherwise time out — and both
        /// of the contracts under test here are success paths: the serial is
        /// written only after the Hub accepts the registration, and
        /// `device_meta` is dropped only after it accepts the removal. `n`
        /// exists because removal is **two** commands (§22.8).
        async fn with_signed_successes<F, Fut>(
            state: &crate::http::AppState,
            n: usize,
            act: F,
        ) -> axum::response::Response
        where
            F: FnOnce() -> Fut + Send + 'static,
            Fut: std::future::Future<Output = axum::response::Response> + Send,
        {
            with_signed_outcomes(state, &vec!["SUCCESS"; n], || async {}, act).await
        }

        /// [`with_signed_successes`] with the Hub's final status per command
        /// (`"ERROR"` refuses it), and `on_wire`, run each time a command is
        /// on the wire and not answered yet: the moment the Hub already has it.
        async fn with_signed_outcomes<F, Fut, W, WFut>(
            state: &crate::http::AppState,
            statuses: &[&str],
            on_wire: W,
            act: F,
        ) -> axum::response::Response
        where
            F: FnOnce() -> Fut + Send + 'static,
            Fut: std::future::Future<Output = axum::response::Response> + Send,
            W: Fn() -> WFut,
            WFut: std::future::Future<Output = ()>,
        {
            let (relay_tx, relay_rx) = tokio::sync::mpsc::unbounded_channel();
            let run_relay = state.relay.clone();
            tokio::spawn(async move { run_relay.run(relay_rx).await });

            // The tape is cumulative across calls, and one test drives this
            // twice (register, then remove) — so start from where the last
            // call left off, or the first `wait_for` matches a frame from the
            // previous command and every challenge afterwards names the wrong
            // request.
            let mut seen = state.relay.published().len();
            let task = tokio::spawn(async move { act().await });
            for (i, status) in statuses.iter().enumerate() {
                let request = wait_for(state, "commands/config/request", seen)
                    .await
                    .unwrap_or_else(|| panic!("command {i} never reached the wire"));
                on_wire().await;
                let request_id = request["request_id"]
                    .as_str()
                    .expect("request_id")
                    .to_string();
                seen = state.relay.published().len();

                // Unique per handshake, not just per `i`: this helper is
                // called twice in one test on the same relay, and the relay
                // keeps a `challenge_id` seen-set for 300 s (design.md
                // §6.2) — a reused id reads as a redelivery and is
                // deliberately not confirmed a second time.
                let challenge_id = format!("chal-{}-{i}", uuid::Uuid::new_v4());
                relay_tx
                    .send(IngestEvent::ConfigChallenge {
                        hostname: HOST.into(),
                        payload: fiber_protocol::payloads::config_challenge::ConfigChallengeMsg {
                            challenge_id: challenge_id.clone(),
                            request_id: Some(request_id),
                            signer_id: None,
                            expires_at: None,
                            preview: None,
                        },
                    })
                    .expect("challenge");
                wait_for(state, "commands/config/confirm", seen)
                    .await
                    .unwrap_or_else(|| panic!("the portal never confirmed command {i}"));
                seen = state.relay.published().len();

                relay_tx
                    .send(IngestEvent::ConfigResponse {
                        hostname: HOST.into(),
                        payload: fiber_protocol::payloads::config_response::ConfigResponseMsg {
                            challenge_id,
                            request_id: None,
                            status: (*status).into(),
                            applied_at: None,
                            effective_at: None,
                            message: Some("done".into()),
                            result: None,
                        },
                    })
                    .expect("response");
            }

            task.await.expect("handler")
        }

        /// Every `command_type` the relay was asked to publish, in order.
        fn commands_published(state: &crate::http::AppState) -> Vec<String> {
            state
                .relay
                .published()
                .into_iter()
                .filter(|(topic, _)| topic.ends_with("commands/config/request"))
                .filter_map(|(_, body)| body["command_type"].as_str().map(str::to_string))
                .collect()
        }

        /// GitLab #80: "band active" is the portal's own flag. Unticking it takes the band out
        /// of the Hub's sync document, which the Hub applies as a delete (config sync
        /// cut-over, amendment 2026-10-15 B: nothing is relayed). The bounds stay in the
        /// portal's row, so ticking the band back on does not ask for them again.
        #[tokio::test]
        async fn unticking_a_band_deletes_it_on_the_device_and_keeps_its_bounds() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "mgr1").await;
            state.store.apply(IngestEvent::Status {
                hostname: HOST.into(),
                status: DeviceStatus::Online,
            });
            pair_hub(&state).await;
            seed_beacon(&state, "AA:BB:CC:DD:EE:01");
            let router = super::super::router().with_state(state.clone());
            let url = "/beacons/AA%3ABB%3ACC%3ADD%3AEE%3A01/thresholds/humidity";
            let bounds = "critical_low=10&warning_low=20&warning_high=80&critical_high=90";
            let key = "beacon/AA:BB:CC:DD:EE:01/threshold/humidity";
            let in_doc = |state: crate::http::AppState| async move {
                crate::sync::desired_entries(&state.db, HOST)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|e| e.key == key)
            };

            let on = post(&router, url, &manager, &format!("{bounds}&enabled=true")).await;
            assert_eq!(on.status(), StatusCode::OK);
            let item = in_doc(state.clone())
                .await
                .expect("ticked: in the document");
            assert_eq!(item.command, "set_eye_field_threshold");

            let off = post(&router, url, &manager, bounds).await;
            assert_eq!(off.status(), StatusCode::OK);
            assert!(
                in_doc(state.clone()).await.is_none(),
                "unticked: out of the document"
            );
            assert!(commands_published(&state).is_empty(), "nothing relayed");

            let rows = state
                .db
                .run(move |s| {
                    s.list_field_thresholds(
                        HOST,
                        portal_storage::THRESHOLD_SCOPE_EYE,
                        "AA:BB:CC:DD:EE:01",
                    )
                })
                .await
                .expect("read back");
            let row = rows
                .iter()
                .find(|r| r.field == "humidity")
                .expect("the row survives");
            assert!(!row.enabled, "switched off");
            assert_eq!(row.critical_high, Some(90.0), "bounds kept for a re-tick");
        }

        /// The Hub's snapshot of a tag it graded CRITICAL, run through the
        /// portal's alarm engine as persist would run it.
        async fn grade_critical(state: &crate::http::AppState, mac: &str, temperature: f64) {
            let mut tag = eye_tag(mac);
            tag.temperature_c = Some(temperature);
            tag.field_alarm_states =
                std::collections::HashMap::from([("temperature".into(), "CRITICAL".into())]);
            state
                .db
                .write(move |s| {
                    let (ntx, _nrx) = tokio::sync::mpsc::unbounded_channel();
                    let (gtx, _grx) = tokio::sync::broadcast::channel(4);
                    crate::persist::run_eye_alarm_engine(
                        s,
                        &Default::default(),
                        HOST,
                        &tag,
                        &ntx,
                        &gtx,
                    )
                })
                .await
                .expect("engine");
        }

        async fn open_alarm(
            state: &crate::http::AppState,
            sensor_id: &'static str,
        ) -> Option<portal_storage::AlarmEvent> {
            state
                .db
                .run(move |s| s.active_alarm(HOST, sensor_id))
                .await
                .expect("read")
        }

        /// decisions.md #28a (UX-040b): the portal's copy of a band is stored
        /// before the command leaves, so a tag the Hub grades against the new
        /// band at once opens its alarm with the limit and the side, and
        /// nothing has to write them into the row afterwards.
        #[tokio::test]
        async fn a_band_is_mirrored_before_the_hub_can_grade_against_it() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "mgr1").await;
            state.store.apply(IngestEvent::Status {
                hostname: HOST.into(),
                status: DeviceStatus::Online,
            });
            pair_hub(&state).await;
            seed_beacon(&state, "AA:BB:CC:DD:EE:01");
            let router = super::super::router().with_state(state.clone());

            let body = "critical_low=-30&warning_low=-25&warning_high=-15&critical_high=-10\
                        &enabled=true";
            let resp = post(
                &router,
                "/beacons/AA%3ABB%3ACC%3ADD%3AEE%3A01/thresholds/temperature",
                &manager,
                body,
            )
            .await;
            // The Hub pulls the band and may grade against it at once.
            grade_critical(&state, "AA:BB:CC:DD:EE:01", 4.4).await;
            assert_eq!(resp.status(), StatusCode::OK);

            let row = open_alarm(&state, "eye:AA:BB:CC:DD:EE:01:Temperature")
                .await
                .expect("the immediate grade opened a row");
            assert_eq!(row.threshold, Some(-10.0), "opened with the new limit");
            assert_eq!(row.alarm_type, "HIGH");
        }

        /// Decision #41 (amended 2026-10-15): the portal's copy is the band. One
        /// the Hub refused stays stored and graded, and the answer says the
        /// device's copy was not delivered rather than a bare failure.
        #[tokio::test]
        async fn a_refused_band_stays_in_the_portal() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "mgr1").await;
            state.store.apply(IngestEvent::Status {
                hostname: HOST.into(),
                status: DeviceStatus::Online,
            });
            pair_hub(&state).await;
            seed_beacon(&state, "AA:BB:CC:DD:EE:01");
            let router = super::super::router().with_state(state.clone());
            let url = "/beacons/AA%3ABB%3ACC%3ADD%3AEE%3A01/thresholds/temperature";

            let first = post(
                &router,
                url,
                &manager,
                "warning_high=5&critical_high=8&enabled=true",
            )
            .await;
            assert_eq!(first.status(), StatusCode::OK);

            let mut status_rx = state.relay.subscribe_status();
            let refused = post(
                &router,
                url,
                &manager,
                "warning_high=6&critical_high=9&enabled=true",
            )
            .await;
            let body = body_string(refused).await;
            assert!(body.contains(r#"data-status="saved""#), "{body}");

            // The Hub's ack fails the key: the save settles as "stored", not as a failure.
            let key = "beacon/AA:BB:CC:DD:EE:01/threshold/temperature".to_string();
            let failed = std::collections::HashMap::from([(key, "rejected".to_string())]);
            state.relay.settle_sync_saves(HOST, u64::MAX, &failed);
            let settled = status_rx.try_recv().expect("the save settles");
            assert_eq!(settled.status, "stored");
            assert_eq!(settled.error_code, Some("device_copy_not_delivered"));

            let band = read_band(&state).await.expect("the row stays");
            assert_eq!(band.critical_high, Some(9.0), "refused: the new band stays");
            assert_eq!(band.warning_high, Some(6.0));
            let history = state
                .db
                .run(|s| {
                    s.threshold_history(
                        HOST,
                        portal_storage::THRESHOLD_SCOPE_EYE,
                        "AA:BB:CC:DD:EE:01",
                        Some("temperature"),
                    )
                })
                .await
                .expect("history");
            assert_eq!(history.len(), 2, "both saves are history: {history:?}");
        }

        async fn read_band(
            state: &crate::http::AppState,
        ) -> Option<portal_storage::FieldThreshold> {
            state
                .db
                .run(|s| {
                    s.get_field_threshold(
                        HOST,
                        portal_storage::THRESHOLD_SCOPE_EYE,
                        "AA:BB:CC:DD:EE:01",
                        "temperature",
                    )
                })
                .await
                .expect("read band")
        }

        /// threshold_defaults.md D5 and the bench check of 2026-09-28: a band saved with
        /// "Threshold enabled" unticked where the Hub holds none — no row, or a default band
        /// that is off — is stored off and signs nothing. Only a band that is on needs the
        /// delete (the test above).
        #[tokio::test]
        async fn a_band_saved_off_where_the_hub_holds_none_sends_nothing() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "mgr1").await;
            state.store.apply(IngestEvent::Status {
                hostname: HOST.into(),
                status: DeviceStatus::Online,
            });
            pair_hub(&state).await;
            seed_beacon(&state, "AA:BB:CC:DD:EE:01");
            ({
                let d = crate::threshold_defaults::configured();
                state.db.write(move |s| {
                    crate::threshold_defaults::seed(
                        s,
                        &d,
                        HOST,
                        portal_storage::THRESHOLD_SCOPE_EYE,
                        "AA:BB:CC:DD:EE:01",
                        ["temperature"],
                    )
                })
            })
            .await
            .expect("a default band");
            let router = super::super::router().with_state(state.clone());

            for field in ["temperature", "humidity"] {
                let response = post(
                    &router,
                    &format!("/beacons/AA%3ABB%3ACC%3ADD%3AEE%3A01/thresholds/{field}"),
                    &manager,
                    "critical_low=-5&warning_low=0&warning_high=30&critical_high=35",
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK, "{field}");
                let html = body_string(response).await;
                assert!(
                    html.contains(&crate::routes::tr("monitoring.threshold.saved_off")),
                    "{html}"
                );
            }
            assert!(commands_published(&state).is_empty(), "nothing is signed");
            let rows = state
                .db
                .run(|s| {
                    s.list_field_thresholds(
                        HOST,
                        portal_storage::THRESHOLD_SCOPE_EYE,
                        "AA:BB:CC:DD:EE:01",
                    )
                })
                .await
                .expect("read");
            assert_eq!(rows.len(), 2);
            for row in rows {
                assert!(!row.enabled, "{row:?}");
                assert_eq!(row.critical_high, Some(35.0), "{row:?}");
            }
        }

        /// devices.beacon.md §4 and §5, end to end: register by **SN + MAC**,
        /// then remove — one command each.
        ///
        /// Registration is `manage` and removal is `destructive`
        /// (plan/portal/mqtt/design.md §4.2), so the actor here is an admin,
        /// who may do both. A manager's half is
        /// `a_manager_is_refused_a_beacon_removal` below.
        #[tokio::test]
        async fn an_admin_can_register_and_remove_a_beacon() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Admin, "admin1").await;
            state.store.apply(IngestEvent::Status {
                hostname: HOST.into(),
                status: DeviceStatus::Online,
            });
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            let router_for_add = router.clone();
            let token = manager.clone();
            let added = with_signed_successes(&state, 1, move || async move {
                post(
                    &router_for_add,
                    "/beacons/add",
                    &token,
                    "serial=6845019001&mac=AA%3ABB%3ACC%3ADD%3AEE%3A01&name=Freezer",
                )
                .await
            })
            .await;
            assert_eq!(added.status(), StatusCode::SEE_OTHER);
            assert_eq!(
                added
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/beacons?ok=added")
            );
            assert_eq!(commands_published(&state), vec!["beacon-add".to_string()]);

            // Its default bands, switched off, and nothing more on the wire
            // (threshold_defaults.md §3).
            let defaults = state
                .db
                .run(|s| {
                    s.list_field_thresholds(
                        HOST,
                        portal_storage::THRESHOLD_SCOPE_EYE,
                        "AA:BB:CC:DD:EE:01",
                    )
                })
                .await
                .expect("read");
            let fields: Vec<&str> = defaults.iter().map(|r| r.field.as_str()).collect();
            assert_eq!(fields, ["humidity", "temperature"]);
            assert!(defaults.iter().all(|r| !r.enabled), "{defaults:?}");
            let t = defaults
                .iter()
                .find(|r| r.field == "temperature")
                .expect("row");
            assert_eq!(
                (
                    t.critical_low,
                    t.warning_low,
                    t.warning_high,
                    t.critical_high
                ),
                (Some(-10.0), Some(0.0), Some(40.0), Some(50.0))
            );

            // The serial is portal-side metadata: it never travelled, and it
            // is the only place the number can live (the advertisement
            // carries none).
            let meta = ({
                let a0 = HOST.to_string();
                let a1 = portal_storage::THRESHOLD_SCOPE_EYE.to_string();
                let a2 = "AA:BB:CC:DD:EE:01".to_string();
                state.db.run(move |s| s.get_device_meta(&a0, &a1, &a2))
            })
            .await
            .expect("read")
            .expect("a row was written");
            assert_eq!(meta.serial.as_deref(), Some("6845019001"));

            // ...and it is what the detail header shows.
            seed_beacon(&state, "AA:BB:CC:DD:EE:01");
            let page =
                body_string(get(&router, "/beacons/AA:BB:CC:DD:EE:01/overview", &manager).await)
                    .await;
            assert!(page.contains("6845019001"), "{page}");

            // A bound on the Beacon, which the removal must take with it.
            ({
                let row = portal_storage::NewFieldThreshold {
                    hostname: HOST.into(),
                    scope: portal_storage::THRESHOLD_SCOPE_EYE.into(),
                    entity: "AA:BB:CC:DD:EE:01".into(),
                    field: "humidity".into(),
                    critical_low: Some(10.0),
                    warning_low: Some(20.0),
                    warning_high: Some(80.0),
                    critical_high: Some(90.0),
                    enabled: true,
                };
                state
                    .db
                    .write(move |s| s.upsert_field_threshold(&row, Some("manager1")))
            })
            .await
            .expect("seed a threshold");

            // Removal: one signed verb, and `device_meta` goes with the
            // device. The pre-step is gone (Matej, 2026-09-15) — see
            // `remove_beacon`'s doc comment for why it could never do the
            // job it was added for.
            let before = commands_published(&state).len();
            let router_for_remove = router.clone();
            let token = manager.clone();
            with_signed_successes(&state, 1, move || async move {
                post(
                    &router_for_remove,
                    "/beacons/AA:BB:CC:DD:EE:01/remove",
                    &token,
                    "confirm=AA:BB:CC:DD:EE:01",
                )
                .await
            })
            .await;
            assert_eq!(
                commands_published(&state)[before..],
                ["beacon-remove".to_string()],
                "removal publishes exactly one verb"
            );
            assert!(
                ({
                    let a0 = HOST.to_string();
                    let a1 = portal_storage::THRESHOLD_SCOPE_EYE.to_string();
                    let a2 = "AA:BB:CC:DD:EE:01".to_string();
                    state.db.run(move |s| s.get_device_meta(&a0, &a1, &a2))
                })
                .await
                .expect("read")
                .is_none(),
                "device_meta goes with the device (§19, F7) — the serial too"
            );
            assert!(
                ({
                    let a0 = HOST.to_string();
                    let a1 = portal_storage::THRESHOLD_SCOPE_EYE.to_string();
                    let a2 = "AA:BB:CC:DD:EE:01".to_string();
                    state
                        .db
                        .run(move |s| s.list_field_thresholds(&a0, &a1, &a2))
                })
                .await
                .expect("read")
                .is_empty(),
                "and so do its thresholds — the removal dialog promises it"
            );

            for event in ["beacon_add", "beacon_remove"] {
                let rows = audit_rows(&state, event).await;
                assert_eq!(rows.len(), 1, "{event} is audited exactly once");
                assert!(rows[0].success, "{event} succeeded");
            }
        }

        /// The typed MAC is checked HERE, not in the browser — the whole
        /// point of moving removal off the list row. A mismatch publishes
        /// nothing and leaves a refusal on the trail, so "someone tried and
        /// got the identifier wrong" is readable after the fact.
        #[tokio::test]
        async fn removal_refuses_a_mismatched_confirmation() {
            let (state, _dir) = test_state().await;
            // Admin: `beacon-remove` is `destructive`, and the gate would
            // refuse a manager before the confirmation is ever read.
            let admin = login_as_role(&state, Role::Admin, "admin1").await;
            state.store.apply(IngestEvent::Status {
                hostname: HOST.into(),
                status: DeviceStatus::Online,
            });
            seed_beacon(&state, "AA:BB:CC:DD:EE:01");
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                "/beacons/AA:BB:CC:DD:EE:01/remove",
                &admin,
                "confirm=AA:BB:CC:DD:EE:02",
            )
            .await;
            assert_eq!(
                response
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/beacons/AA:BB:CC:DD:EE:01/overview?error=confirm_mismatch")
            );
            assert!(state.relay.published().is_empty());

            // UX-044: the Overview tab it lands on shows the refusal.
            let page = body_string(
                get(
                    &router,
                    "/beacons/AA:BB:CC:DD:EE:01/overview?error=confirm_mismatch",
                    &admin,
                )
                .await,
            )
            .await;
            assert!(
                page.contains("That is not this Beacon&#x27;s MAC.")
                    || page.contains("That is not this Beacon's MAC."),
                "no refusal on the page"
            );

            let rows = audit_rows(&state, "beacon_remove").await;
            assert_eq!(rows.len(), 1);
            assert!(!rows[0].success);
        }

        /// §4: "duplicitní registrace (stejná MAC) → chyba, ne falešný
        /// úspěch". A *discovered* MAC is the normal case — that is what the
        /// scan table is for — so only the enrolled half is refused.
        #[tokio::test]
        async fn registering_a_mac_that_is_already_registered_is_an_error_not_a_redirect_to_ok() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_scan(&state);
            let router = super::super::router().with_state(state.clone());

            let refused = post(
                &router,
                "/beacons/add",
                &manager,
                "serial=1&mac=AA%3ABB%3ACC%3ADD%3AEE%3A01&name=x",
            )
            .await;
            assert_eq!(refused.status(), StatusCode::SEE_OTHER);
            assert_eq!(
                refused
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/beacons/add?error=already_registered")
            );
            assert!(
                audit_rows(&state, "beacon_add").await.is_empty(),
                "nothing was attempted, so nothing is audited"
            );
            assert!(
                state.relay.published().is_empty(),
                "and nothing reached the wire"
            );

            // The discovered MAC beside it is registrable — refusing that
            // would refuse the whole point of the scan table.
            let accepted = post(
                &router,
                "/beacons/add",
                &manager,
                "serial=1&mac=AA%3ABB%3ACC%3ADD%3AEE%3A02&name=x",
            )
            .await;
            assert_eq!(accepted.status(), StatusCode::SEE_OTHER);
            assert_ne!(
                accepted
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/beacons/add?error=already_registered")
            );

            // A missing serial is refused before anything else happens.
            let no_serial = post(
                &router,
                "/beacons/add",
                &manager,
                "serial=+&mac=AA%3ABB%3ACC%3ADD%3AEE%3A03&name=x",
            )
            .await;
            assert_eq!(
                no_serial
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/beacons/add?error=serial_required")
            );
        }

        /// §3.5's single BLE-half action. The command times out here — no Hub
        /// answers `test_state`'s relay — which is the point: the outcome
        /// reaches `#command-status` and the attempt is audited either way.
        #[tokio::test(start_paused = true)]
        async fn reconfigure_renders_the_command_status_fragment_and_audits() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_scan(&state);
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                "/beacons/AA:BB:CC:DD:EE:01/reconfigure",
                &manager,
                "",
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = body_string(response).await;
            assert!(
                body.contains("command-status"),
                "the outcome goes into the shared shell: {body}"
            );
            assert_eq!(
                commands_published(&state),
                vec!["beacon-provision".to_string()]
            );

            let rows = audit_rows(&state, "beacon_reconfigure").await;
            assert_eq!(rows.len(), 1);
            assert!(!rows[0].success, "a timeout is an audited failure");
            assert_eq!(rows[0].target.as_deref(), Some("AA:BB:CC:DD:EE:01"));
        }

        /// The same shape for the history request (§3.8, WP6.5): a signed
        /// command, an outcome in `#command-status`, an audit row — and no
        /// invented progress states.
        #[tokio::test(start_paused = true)]
        async fn requesting_history_is_one_signed_command_and_an_audit_row() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_scan(&state);
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            post(&router, "/beacons/AA:BB:CC:DD:EE:01/history", &manager, "").await;
            assert_eq!(
                commands_published(&state),
                vec!["beacon-history".to_string()]
            );
            let rows = audit_rows(&state, "beacon_history_request").await;
            assert_eq!(rows.len(), 1);
        }

        async fn beacon_jobs(
            state: &crate::http::AppState,
        ) -> Vec<portal_storage::BeaconHistoryJob> {
            state
                .db
                .run(|s| s.list_beacon_history_jobs("AA:BB:CC:DD:EE:01", None))
                .await
                .expect("jobs")
        }

        /// beacon_history.md §2.1/§4.3: a window goes out in the signed
        /// request with the job's id, one job runs per Beacon, and a job the
        /// Hub has not picked up can be cancelled — after which a new one may
        /// start.
        #[tokio::test(start_paused = true)]
        async fn a_window_request_carries_its_job_and_one_runs_at_a_time() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_scan(&state);
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());
            let path = "/beacons/AA:BB:CC:DD:EE:01/history";

            post(&router, path, &manager, "preset=6h").await;
            let sent = state.relay.published();
            let params = &sent
                .iter()
                .find(|(topic, _)| topic.ends_with("commands/config/request"))
                .expect("request published")
                .1["params"];
            let jobs = beacon_jobs(&state).await;
            assert_eq!(jobs.len(), 1);
            let job = &jobs[0];
            assert_eq!(params["mac"], "AA:BB:CC:DD:EE:01");
            assert_eq!(params["job"], job.id.as_str());
            assert_eq!(params["from"].as_i64(), job.from_ts);
            assert_eq!(params["to"].as_i64(), job.to_ts);
            assert_eq!(job.to_ts.unwrap() - job.from_ts.unwrap(), 6 * 3600);
            assert_eq!(job.trigger, "manual");
            assert_eq!(job.created_by.as_deref(), Some("manager1"));
            // Nobody answered: the job waits, cancellable, for the reaper.
            assert_eq!(job.state, "pending");

            // A second request while it runs is refused and sends nothing.
            let refused = post(&router, path, &manager, "preset=1h").await;
            assert_eq!(refused.status(), StatusCode::CONFLICT);
            assert_eq!(commands_published(&state).len(), 1);
            assert_eq!(beacon_jobs(&state).await.len(), 1);

            let cancel = format!("{path}/{}/cancel", job.id);
            let table = body_string(post(&router, &cancel, &manager, "").await).await;
            assert!(table.contains(r#"id="beacon-history-AA-BB-CC-DD-EE-01""#));
            assert_eq!(beacon_jobs(&state).await[0].state, "cancelled");
            let rows = audit_rows(&state, "beacon_history_cancel").await;
            assert_eq!(rows.len(), 1);
            assert!(rows[0].success);

            post(&router, path, &manager, "preset=1h").await;
            assert_eq!(commands_published(&state).len(), 2);
            assert_eq!(beacon_jobs(&state).await.len(), 2);
        }

        /// The History tab is the Node's (§7.1): the form, the quick choices
        /// and the shared table under this Beacon's swap id; Diagnostics no
        /// longer carries a history card.
        #[tokio::test]
        async fn the_history_tab_is_the_nodes_card() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_scan(&state);
            let router = super::super::router().with_state(state.clone());

            let page =
                body_string(get(&router, "/beacons/AA:BB:CC:DD:EE:01/history", &manager).await)
                    .await
                    .replace("&#x2f;", "/");
            assert!(page.contains(r#"hx-post="/beacons/AA:BB:CC:DD:EE:01/history""#));
            assert!(page.contains(r#"name="from""#));
            let zone = format!("({})</label>", crate::display_tz::name());
            assert_eq!(
                page.matches(&zone).count(),
                2,
                "the window is labelled with the display zone"
            );
            assert!(page.contains(r#"sse-swap="beacon-history-AA-BB-CC-DD-EE-01""#));
            let diagnostics =
                body_string(get(&router, "/beacons/AA:BB:CC:DD:EE:01/diagnostics", &manager).await)
                    .await
                    .replace("&#x2f;", "/");
            assert!(!diagnostics.contains(r#"hx-post="/beacons/AA:BB:CC:DD:EE:01/history""#));
        }

        /// §4.2's switch: saved at once into `device_meta.backfill_auto`
        /// (scope `eye`), audited, and nothing sent to the Hub.
        #[tokio::test]
        async fn the_automatic_history_switch_saves_and_sends_nothing() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_scan(&state);
            let router = super::super::router().with_state(state.clone());

            // An unticked checkbox posts nothing.
            post(
                &router,
                "/beacons/AA:BB:CC:DD:EE:01/history-auto",
                &manager,
                "",
            )
            .await;
            let row = state
                .db
                .run(|s| {
                    s.get_device_meta(
                        "h1",
                        portal_storage::THRESHOLD_SCOPE_EYE,
                        "AA:BB:CC:DD:EE:01",
                    )
                })
                .await
                .expect("read")
                .expect("row");
            assert!(!row.backfill_auto);
            assert!(commands_published(&state).is_empty());
            let rows = audit_rows(&state, "beacon_history_auto_set").await;
            assert_eq!(rows.len(), 1);
            assert!(rows[0].success);
        }

        /// B6 review, item 3. `serial_recorded` is a Class IIa claim about
        /// this device's registration, so it has to be the outcome of the
        /// write and not a constant written before it.
        #[tokio::test(start_paused = true)]
        async fn the_serial_audit_reports_the_write_that_actually_happened() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: HOST.into(),
                status: DeviceStatus::Online,
            });
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            // Nobody answers this relay, so the registration times out —
            // nothing was registered, so there was no serial to record and
            // the row makes no claim either way.
            let refused = post(
                &router,
                "/beacons/add",
                &manager,
                "serial=6845019001&mac=AA%3ABB%3ACC%3ADD%3AEE%3A01&name=Freezer",
            )
            .await;
            assert_eq!(
                refused
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/beacons/add?error=add_failed")
            );
            let rows = audit_rows(&state, "beacon_add").await;
            assert_eq!(rows.len(), 1);
            assert!(!rows[0].success, "a registration that did not happen");
            let detail = rows[0].details.as_deref().unwrap_or_default();
            assert!(
                !detail.contains("serial_recorded"),
                "a refused registration must claim nothing about the serial: {detail}"
            );
        }

        fn location(response: &axum::response::Response) -> Option<&str> {
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .and_then(|v| v.to_str().ok())
        }

        /// GitLab #109: on a Hub that is not paired nothing can be signed, and the operator was
        /// told "Adding the beacon failed — check the MQTT connection". The refusal names the
        /// pairing now; it is still not audited, because nothing was sent (`Cmd::relay`'s rule).
        #[tokio::test]
        async fn an_unpaired_hub_refuses_beacon_add_as_not_paired_not_as_mqtt() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: HOST.into(),
                status: DeviceStatus::Online,
            });
            let router = super::super::router().with_state(state.clone());

            let refused = post(
                &router,
                "/beacons/add",
                &manager,
                "serial=6845019001&mac=AA%3ABB%3ACC%3ADD%3AEE%3A01&name=Freezer",
            )
            .await;
            assert_eq!(location(&refused), Some("/beacons/add?error=not_paired"));
            assert!(state.relay.published().is_empty(), "nothing was signed");
            assert!(audit_rows(&state, "beacon_add").await.is_empty());

            let page =
                body_string(get(&router, "/beacons/add?error=not_paired", &manager).await).await;
            assert!(
                page.contains(&crate::i18n::lookup(
                    "en",
                    "monitoring.shared.signing_key_not_paired"
                )),
                "the page says the Hub is not paired"
            );
            assert!(
                !page.contains("check the MQTT connection"),
                "an unpaired Hub is not an MQTT failure"
            );
        }

        /// The removal half of #109: the refusal reaches the Beacon's page as the pairing, not
        /// as "could not be removed", and nothing is audited or published.
        #[tokio::test]
        async fn an_unpaired_hub_refuses_beacon_removal_as_not_paired() {
            let (state, _dir) = test_state().await;
            let admin = login_as_role(&state, Role::Admin, "admin1").await;
            state.store.apply(IngestEvent::Status {
                hostname: HOST.into(),
                status: DeviceStatus::Online,
            });
            seed_beacon(&state, "AA:BB:CC:DD:EE:01");
            let router = super::super::router().with_state(state.clone());

            let refused = post(
                &router,
                "/beacons/AA:BB:CC:DD:EE:01/remove",
                &admin,
                "confirm=AA:BB:CC:DD:EE:01",
            )
            .await;
            assert_eq!(
                location(&refused),
                Some("/beacons/AA:BB:CC:DD:EE:01/overview?error=not_paired")
            );
            assert!(state.relay.published().is_empty());
            assert!(audit_rows(&state, "beacon_remove").await.is_empty());

            let page = body_string(
                get(
                    &router,
                    "/beacons/AA:BB:CC:DD:EE:01/overview?error=not_paired",
                    &admin,
                )
                .await,
            )
            .await;
            assert!(page.contains(&crate::i18n::lookup(
                "en",
                "monitoring.shared.signing_key_not_paired"
            )));
        }

        /// #109, said up front: while no Hub can sign a registration the Add page names the
        /// unpaired Hub and disables the form. In a cluster with one paired Hub the form stays
        /// usable — its select can pick that Hub — and only the unpaired one is named.
        #[tokio::test]
        async fn the_add_page_disables_registration_while_no_hub_is_paired() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: HOST.into(),
                status: DeviceStatus::Online,
            });
            let router = super::super::router().with_state(state.clone());
            let disabled =
                r#"<fieldset class="grid grid-cols-1 sm:grid-cols-2 gap-3 mt-3" disabled>"#;

            let page = body_string(get(&router, "/beacons/add", &manager).await).await;
            assert!(page.contains(r#"data-unpaired-hub="h1""#), "{page}");
            assert!(page.contains(disabled), "{page}");

            pair_hub(&state).await;
            state.store.apply(IngestEvent::Status {
                hostname: "h2".into(),
                status: DeviceStatus::Online,
            });
            let page = body_string(get(&router, "/beacons/add", &manager).await).await;
            assert!(!page.contains(r#"data-unpaired-hub="h1""#), "{page}");
            assert!(page.contains(r#"data-unpaired-hub="h2""#), "{page}");
            assert!(
                !page.contains(disabled),
                "a paired Hub can still take it: {page}"
            );
        }

        /// #109, dodatek 2026-10-13: the Beacons list carries the same banner as the Add page,
        /// and for an admin the pairing form itself, returning to the list. A manager, who
        /// cannot pair, is told who can instead of being handed a form the route refuses.
        #[tokio::test]
        async fn the_list_banner_lets_an_admin_pair_right_there() {
            let (state, _dir) = test_state().await;
            let admin = login_as_role(&state, Role::Admin, "admin1").await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: HOST.into(),
                status: DeviceStatus::Online,
            });
            let router = super::super::router().with_state(state.clone());

            let page = body_string(get(&router, "/beacons", &admin).await).await;
            assert!(page.contains(r#"data-unpaired-hub="h1""#), "{page}");
            assert!(page.contains(r#"action="/hubs/h1/pair""#), "{page}");
            assert!(
                page.contains(r#"name="return_to" value="&#x2f;beacons""#),
                "{page}"
            );

            let page = body_string(get(&router, "/beacons", &manager).await).await;
            assert!(page.contains(r#"data-unpaired-hub="h1""#), "{page}");
            assert!(!page.contains(r#"action="/hubs/h1/pair""#), "{page}");

            pair_hub(&state).await;
            let page = body_string(get(&router, "/beacons", &admin).await).await;
            assert!(!page.contains("data-unpaired-hub"), "{page}");
        }

        /// The success half of the same claim, from the walk-through test's
        /// own fixture: the Hub accepted, the row was written, and the audit
        /// says so because the write said so.
        #[tokio::test]
        async fn a_successful_registration_audits_the_serial_it_stored() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: HOST.into(),
                status: DeviceStatus::Online,
            });
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            let token = manager.clone();
            with_signed_successes(&state, 1, move || async move {
                post(
                    &router,
                    "/beacons/add",
                    &token,
                    "serial=6845019001&mac=AA%3ABB%3ACC%3ADD%3AEE%3A01&name=Freezer",
                )
                .await
            })
            .await;

            let rows = audit_rows(&state, "beacon_add").await;
            assert_eq!(rows.len(), 1);
            assert!(rows[0].success);
            let detail = rows[0].details.as_deref().unwrap_or_default();
            assert!(
                detail.contains("\"serial_recorded\":true"),
                "the serial was stored, so the row must say so: {detail}"
            );
            // ...and it really is there, which is what the claim is about.
            assert_eq!(
                ({
                    let a0 = HOST.to_string();
                    let a1 = portal_storage::THRESHOLD_SCOPE_EYE.to_string();
                    let a2 = "AA:BB:CC:DD:EE:01".to_string();
                    state.db.run(move |s| s.get_device_meta(&a0, &a1, &a2))
                })
                .await
                .expect("read")
                .expect("row")
                .serial
                .as_deref(),
                Some("6845019001")
            );
        }

        /// B6 review, item 4. Hiding the button is a courtesy; the route is
        /// the rule. A `read_only` Beacon refuses configuration writes, so
        /// the portal refuses the request instead of spending a signed
        /// command on a guaranteed rejection.
        #[tokio::test(start_paused = true)]
        async fn reconfiguring_a_read_only_beacon_is_refused_server_side() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::EyeSensors {
                hostname: HOST.into(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: false,
                    tags: vec![fiber_protocol::payloads::eye::EyeTag {
                        provisioning: Some("read_only".into()),
                        ..eye_tag("AA:BB:CC:DD:EE:01")
                    }],
                },
            });
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                "/beacons/AA:BB:CC:DD:EE:01/reconfigure",
                &manager,
                "",
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = body_string(response).await;
            assert!(
                body.contains("read-only"),
                "the operator is told why: {body}"
            );
            assert!(
                commands_published(&state).is_empty(),
                "nothing may reach the wire: {:?}",
                commands_published(&state)
            );
            let rows = audit_rows(&state, "beacon_reconfigure").await;
            assert_eq!(rows.len(), 1, "a refusal is still an audited attempt");
            assert!(!rows[0].success);
            assert!(rows[0]
                .details
                .as_deref()
                .unwrap_or_default()
                .contains("read_only"));
        }

        /// `beacon-remove` is `destructive` (plan/portal/mqtt/design.md
        /// §4.2) and the route is `RequireAdmin` to match — design §13 item
        /// 3, ratified with Matej on 2026-09-15 after P2's audit found
        /// `restart_application`, `beacon-remove` and `remove_lorawan_sticker`
        /// all in the same shape: a `destructive` verb behind a
        /// `RequireManager` gate.
        ///
        /// **The gate is what refuses, not `relay::authorize`.** Both would
        /// refuse, and P2's first cut let the manager through to be turned
        /// back at the signing step. That is worse than it looks: the route
        /// had already run, and while removal was still a two-step command
        /// it left the Beacon reconfigured but registered — a half-success
        /// out of a refusal. The pre-step is gone now (§1), so the specific
        /// hazard is too, but the ordering is the rule: refuse before the
        /// handler does anything, and the wire stays clean by construction
        /// rather than by the handler happening to have nothing to undo.
        #[tokio::test]
        async fn a_manager_is_refused_a_beacon_removal() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: HOST.into(),
                status: DeviceStatus::Online,
            });
            state.store.apply(IngestEvent::EyeSensors {
                hostname: HOST.into(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: false,
                    tags: vec![eye_tag("AA:BB:CC:DD:EE:01")],
                },
            });
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            let before = state.relay.published().len();
            let response = post(&router, "/beacons/AA:BB:CC:DD:EE:01/remove", &manager, "").await;

            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "a manager is turned back at the gate, not after the handler has run"
            );
            assert_eq!(
                state.relay.published().len(),
                before,
                "nothing reaches the wire"
            );
            let (_, total) = state
                .auth
                .audit_page(portal_storage::AuditFilter {
                    event_type: Some("beacon_remove".to_string()),
                    ..Default::default()
                })
                .await
                .expect("audit page");
            assert_eq!(total, 0, "and the handler never ran to write a row");
        }

        /// §3.5: a `read_only` Beacon refuses configuration writes, so the
        /// button is hidden rather than offered and rejected. And a `failed`
        /// one gets the retry label plus the "no reason on the wire" line.
        #[tokio::test]
        async fn a_read_only_beacon_offers_no_reconfigure_button() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());

            let page = |mac: &str, provisioning: &str| {
                state.store.apply(IngestEvent::EyeSensors {
                    hostname: HOST.into(),
                    payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                        timestamp: None,
                        discovered_capped: false,
                        tags: vec![fiber_protocol::payloads::eye::EyeTag {
                            provisioning: Some(provisioning.into()),
                            ..eye_tag(mac)
                        }],
                    },
                });
            };

            page("AA:BB:CC:DD:EE:01", "read_only");
            let read_only =
                body_string(get(&router, "/beacons/AA:BB:CC:DD:EE:01/settings", &manager).await)
                    .await
                    .replace("&#x2f;", "/");
            assert!(read_only.contains("read-only"), "{read_only}");
            assert!(
                !read_only.contains("/beacons/AA:BB:CC:DD:EE:01/reconfigure"),
                "a read-only Beacon is not offered a write it will refuse"
            );

            page("AA:BB:CC:DD:EE:02", "failed");
            let failed =
                body_string(get(&router, "/beacons/AA:BB:CC:DD:EE:02/settings", &manager).await)
                    .await
                    .replace("&#x2f;", "/");
            assert!(failed.contains("configuration failed"), "{failed}");
            assert!(failed.contains("Try again"), "{failed}");
            assert!(
                failed.contains("The configuration failed, try again."),
                "no reason travels on the wire; the page points at the retry: {failed}"
            );
            assert!(failed.contains("/beacons/AA:BB:CC:DD:EE:02/reconfigure"));
        }

        /// WP6.6 (now the Channels card) and §3.3's JSON preview — and
        /// the preview's absence before the first message, which is a
        /// different statement from an empty one.
        #[tokio::test]
        async fn the_detail_page_lists_both_fields_and_previews_the_last_message() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::EyeSensors {
                hostname: HOST.into(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: false,
                    tags: vec![fiber_protocol::payloads::eye::EyeTag {
                        temperature_c: Some(-18.5),
                        humidity_pct: Some(41),
                        ..eye_tag("AA:BB:CC:DD:EE:01")
                    }],
                },
            });
            let router = super::super::router().with_state(state.clone());

            let page =
                body_string(get(&router, "/beacons/AA:BB:CC:DD:EE:01/overview", &manager).await)
                    .await
                    .replace("&#x2f;", "/");
            // The Channels card (ui_components.md §2, amended 2026-10-15):
            // both fields plus the battery row in a stats table, no chart,
            // and a Trends link naming the Beacon's channels.
            assert!(
                page.contains("data-channels-card"),
                "no Channels card: {page}"
            );
            for field in ["temperature", "humidity"] {
                assert!(
                    page.contains(&format!("eye:AA:BB:CC:DD:EE:01:{field}")),
                    "{field} is not in the Trends link: {page}"
                );
            }
            assert!(
                !page.contains("/api/v1/chart/eye/") && !page.contains("portal-charts.js"),
                "the card has no chart: {page}"
            );
            assert!(page.contains("-18.5"), "Last of the temperature: {page}");
            // §2.6: the select column is always there, the manager gets
            // Set channels; Payload rows only, the Source column is held
            // back (§2.7).
            assert!(page.contains(r#"<th class="ch-sel""#), "{page}");
            assert!(page.contains("data-edit-set-start"), "{page}");
            assert!(!page.contains("ch-src-"), "{page}");
            assert!(
                page.contains("/beacons/AA:BB:CC:DD:EE:01/sensors/temperature"),
                "{page}"
            );
            // §3.3: the raw tag, expandable and copyable — on Diagnostics.
            let diagnostics =
                body_string(get(&router, "/beacons/AA:BB:CC:DD:EE:01/diagnostics", &manager).await)
                    .await;
            assert!(diagnostics.contains("Message history"), "{diagnostics}");
            assert!(
                diagnostics.contains("temperature_c&quot;: -18.5"),
                "the raw tag, HTML-escaped inside the <pre>: {diagnostics}"
            );

            // A Beacon the store has never had a snapshot for has no payload,
            // and says so rather than rendering an empty box. Asked of the
            // detail route's own builder, which is where the pretty-print
            // lives since the B6 review (item 10) — `beacon_ctx` leaves the
            // field `None` for every caller.
            let beacon = crate::state::BeaconState::new("AA:BB:CC:DD:EE:09".into(), 0);
            assert!(
                crate::render::beacon_last_payload_json(&beacon).is_none(),
                "no message, no preview — not an empty one"
            );
            let rows = super::super::load_beacon_row_data(&state, HOST).await;
            let listed = crate::render::beacon_ctx(rows.inputs(
                &state.store.snapshot_device(HOST).unwrap().ble_sensors["AA:BB:CC:DD:EE:01"],
                Vec::new(),
            ));
            assert!(
                listed.last_payload_json.is_none(),
                "a list render must not pretty-print a preview only the detail page reads"
            );
        }

        /// A viewer may watch the scan and register nothing (§7: the scan
        /// table is visible inside the registration flow, which is
        /// manager+; the read itself is not privileged).
        #[tokio::test]
        async fn a_viewer_sees_the_scan_but_no_form() {
            let (state, _dir) = test_state().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            seed_scan(&state);
            let router = super::super::router().with_state(state.clone());

            let body = body_string(get(&router, "/beacons/add", &viewer).await)
                .await
                .replace("&#x2f;", "/");
            assert!(body.contains("AA:BB:CC:DD:EE:02"), "{body}");
            assert!(
                !body.contains(r#"action="/beacons/add""#),
                "a viewer gets no registration form: {body}"
            );
            assert!(!body.contains("data-mac="), "and no Add buttons: {body}");
        }

        // ---- Decision #42: Beacons belong to the cluster ---------------

        /// A cluster: Server `s1`, Client `c1`. `…:01` is registered and homed on `c1` (it hears
        /// it better); `…:02` is a stranger both Hubs hear.
        fn seed_cluster(state: &crate::http::AppState) {
            for (host, rssi_01, rssi_02) in [("s1", -80, -70), ("c1", -50, -60)] {
                state.store.apply(IngestEvent::Status {
                    hostname: host.into(),
                    status: DeviceStatus::Online,
                });
                state.store.apply(IngestEvent::EyeSensors {
                    hostname: host.into(),
                    payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                        timestamp: None,
                        discovered_capped: false,
                        tags: vec![
                            fiber_protocol::payloads::eye::EyeTag {
                                discovered: None,
                                rssi: Some(rssi_01),
                                ..eye_tag("AA:BB:CC:DD:EE:01")
                            },
                            fiber_protocol::payloads::eye::EyeTag {
                                discovered: Some(true),
                                rssi: Some(rssi_02),
                                ..eye_tag("AA:BB:CC:DD:EE:02")
                            },
                        ],
                    },
                });
            }
            state.store.apply(IngestEvent::EyeHomes {
                hostname: "s1".into(),
                payload: fiber_protocol::payloads::eye::EyeHomesMsg {
                    v: 1,
                    server: "s1".into(),
                    registered: vec!["AA:BB:CC:DD:EE:01".into()],
                    homes: [("AA:BB:CC:DD:EE:01".to_string(), "c1".to_string())].into(),
                },
            });
        }

        async fn pair(state: &crate::http::AppState, host: &str) {
            state
                .keys
                .store_pairing(
                    host,
                    Some("test-ca".into()),
                    "00".repeat(32),
                    [7u8; 32],
                    None,
                    None,
                )
                .await
                .expect("store pairing");
        }

        /// Every Hub the relay was asked to send a command to, in order.
        fn command_hubs(state: &crate::http::AppState) -> Vec<String> {
            state
                .relay
                .published()
                .into_iter()
                .filter(|(topic, _)| topic.ends_with("commands/config/request"))
                .filter_map(|(topic, _)| {
                    let parts: Vec<&str> = topic.split('/').collect();
                    parts.len().checked_sub(4).map(|i| parts[i].to_string())
                })
                .collect()
        }

        /// The list holds each Beacon once, under the Server, with its home Hub named.
        #[tokio::test]
        async fn a_cluster_lists_each_beacon_once_under_the_server_with_its_home() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_cluster(&state);
            let router = super::super::router().with_state(state.clone());

            let body = body_string(get(&router, "/beacons", &manager).await).await;
            assert!(
                body.contains(r#"id="beacon-s1-AA-BB-CC-DD-EE-01""#),
                "{body}"
            );
            assert!(
                !body.contains("beacon-c1-AA-BB-CC-DD-EE-01"),
                "no second row from the Client: {body}"
            );
            assert!(
                body.replace("&#x2f;", "/").contains("Hub / brána")
                    || body.replace("&#x2f;", "/").contains("Hub / Gateway"),
                "the home column: {body}"
            );
            assert!(
                body.contains(r#"<td data-column="hub"><span class="block font-mono text-xs truncate">c1</span>"#),
                "the row names its home: {body}"
            );
        }

        /// The add page has no Hub to pick and one merged scan, a stranger once.
        #[tokio::test]
        async fn a_cluster_add_page_has_no_hub_select_and_one_scan_row_per_mac() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_cluster(&state);
            let router = super::super::router().with_state(state.clone());

            let body = body_string(get(&router, "/beacons/add", &manager).await).await;
            assert!(!body.contains(r#"name="hub""#), "no Hub select: {body}");
            assert!(body.contains("data-cluster-register"), "{body}");
            assert!(
                body.contains(r#"sse-swap="beacons-discovered-cluster""#),
                "one panel, the cluster's: {body}"
            );
            assert_eq!(
                body.matches(r#"data-mac="AA:BB:CC:DD:EE:02""#).count(),
                1,
                "the stranger both Hubs hear is one row: {body}"
            );
            assert!(
                !body.contains(r#"data-mac="AA:BB:CC:DD:EE:01""#),
                "the registered Beacon is no stranger: {body}"
            );
        }

        /// Q6: a registration naming a Client is refused before anything is signed, in the
        /// operator's language, naming both Hubs.
        #[tokio::test]
        async fn a_cluster_refuses_a_registration_naming_a_client() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_cluster(&state);
            pair(&state, "s1").await;
            pair(&state, "c1").await;
            let router = super::super::router().with_state(state.clone());

            let refused = post(
                &router,
                "/beacons/add",
                &manager,
                "serial=1&mac=AA%3ABB%3ACC%3ADD%3AEE%3A02&hub=c1",
            )
            .await;
            assert_eq!(
                location(&refused),
                Some("/beacons/add?error=client_hub&hub=c1")
            );
            assert!(state.relay.published().is_empty(), "nothing signed");

            let body =
                body_string(get(&router, "/beacons/add?error=client_hub&hub=c1", &manager).await)
                    .await;
            let cs = crate::routes::tr("monitoring.beacons.err_client_hub")
                .replace("{server}", "s1")
                .replace("{client}", "c1");
            assert!(body.contains(&cs), "{cs} in {body}");
        }

        /// A registration without a Hub goes to the Server; a registered MAC is refused even
        /// though a Hub without a view might still list it as a stranger.
        #[tokio::test(start_paused = true)]
        async fn a_cluster_registers_on_the_server() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_cluster(&state);
            pair(&state, "s1").await;
            let router = super::super::router().with_state(state.clone());

            let dup = post(
                &router,
                "/beacons/add",
                &manager,
                "serial=1&mac=AA%3ABB%3ACC%3ADD%3AEE%3A01",
            )
            .await;
            assert_eq!(
                location(&dup),
                Some("/beacons/add?error=already_registered")
            );

            post(
                &router,
                "/beacons/add",
                &manager,
                "serial=1&mac=AA%3ABB%3ACC%3ADD%3AEE%3A02",
            )
            .await;
            assert_eq!(command_hubs(&state), vec!["s1".to_string()]);
            assert_eq!(commands_published(&state), vec!["beacon-add".to_string()]);
        }

        /// GATT goes to the home Hub; the history job stays keyed by the Server.
        #[tokio::test(start_paused = true)]
        async fn a_cluster_sends_gatt_commands_to_the_home_hub() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_cluster(&state);
            pair(&state, "s1").await;
            pair(&state, "c1").await;
            let router = super::super::router().with_state(state.clone());

            post(
                &router,
                "/beacons/AA:BB:CC:DD:EE:01/reconfigure",
                &manager,
                "",
            )
            .await;
            post(&router, "/beacons/AA:BB:CC:DD:EE:01/history", &manager, "").await;
            assert_eq!(
                command_hubs(&state),
                vec!["c1".to_string(), "c1".to_string()]
            );
            let jobs = beacon_jobs(&state).await;
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].hostname, "s1", "the job is the registry's");
        }
    }

    // ---- Two Hubs in the store (devices.hub.md §12 E) --------------------

    mod multi_hub {
        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use fiber_protocol::payloads::status::DeviceStatus;
        use tower::ServiceExt;

        use super::integration::eye_tag;
        use crate::auth::{Role, SESSION_COOKIE};
        use crate::routes::test_support::{login, login_as_role, test_state};
        use crate::state::events::IngestEvent;

        fn seed(state: &crate::http::AppState, hostname: &str, mac: &str) {
            state.store.apply(IngestEvent::Status {
                hostname: hostname.into(),
                status: DeviceStatus::Online,
            });
            state.store.apply(IngestEvent::EyeSensors {
                hostname: hostname.into(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: false,
                    tags: vec![eye_tag(mac)],
                },
            });
        }

        /// `/beacons` is one list over the whole fleet, each row keyed by the
        /// Hub it is registered on.
        #[tokio::test]
        async fn the_list_holds_every_hubs_beacons() {
            let (state, _dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            seed(&state, "h1", "AA:BB:CC:DD:EE:01");
            seed(&state, "h2", "AA:BB:CC:DD:EE:02");
            let router = super::super::router().with_state(state.clone());

            let response = router
                .oneshot(
                    Request::builder()
                        .uri("/beacons")
                        .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            let html = String::from_utf8(body.to_vec()).expect("utf8");

            assert!(html.contains("beacon-h1-AA-BB-CC-DD-EE-01"), "h1's Beacon");
            assert!(html.contains("beacon-h2-AA-BB-CC-DD-EE-02"), "h2's Beacon");
        }

        /// The metadata write lands on the Hub that has the MAC registered,
        /// not on the first Hub in the `BTreeMap` — `h1` sorts first and has
        /// no Beacon at all.
        /// devices.beacon.md §4: every Hub scans, and the row names the Hub
        /// that heard it. The page used to take `scoped_hub` for both halves,
        /// which in a cluster without `?hub=` meant no scan panel at all and a
        /// registration with nowhere to go; now the form asks which Hub, and a
        /// scan row's Add button preselects the one that heard the Beacon.
        #[tokio::test]
        async fn the_add_page_scans_every_hub_and_asks_which_one_registers() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            for (host, mac) in [("h1", "AA:BB:CC:DD:EE:11"), ("h2", "AA:BB:CC:DD:EE:22")] {
                state.store.apply(IngestEvent::Status {
                    hostname: host.into(),
                    status: DeviceStatus::Online,
                });
                state.store.apply(IngestEvent::EyeSensors {
                    hostname: host.into(),
                    payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                        timestamp: None,
                        discovered_capped: false,
                        tags: vec![fiber_protocol::payloads::eye::EyeTag {
                            discovered: Some(true),
                            rssi: Some(-60),
                            ..eye_tag(mac)
                        }],
                    },
                });
            }
            let router = super::super::router().with_state(state.clone());

            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/beacons/add")
                        .header(header::COOKIE, format!("{SESSION_COOKIE}={manager}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            let html = String::from_utf8(body.to_vec()).expect("utf8");
            assert!(html.contains("AA:BB:CC:DD:EE:11"), "h1's scan: {html}");
            assert!(html.contains("AA:BB:CC:DD:EE:22"), "h2's scan: {html}");
            assert!(
                html.contains(r#"name="hub""#),
                "the form asks which Hub: {html}"
            );
            assert!(
                html.contains(r#"data-hub="h2""#),
                "Add preselects the hearing Hub: {html}"
            );

            // Posting without a Hub is a choice to make, not "nothing reported".
            let response = router
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/beacons/add")
                        .header(header::COOKIE, format!("{SESSION_COOKIE}={manager}"))
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from("serial=1&mac=AA:BB:CC:DD:EE:22"))
                        .unwrap(),
                )
                .await
                .expect("response");
            assert_eq!(
                response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/beacons/add?error=choose_hub")
            );
        }

        #[tokio::test]
        async fn a_beacon_write_lands_on_the_owning_hub() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "h1".into(),
                status: DeviceStatus::Online,
            });
            seed(&state, "h2", "AA:BB:CC:DD:EE:02");
            let router = super::super::router().with_state(state.clone());

            let response = router
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/beacons/aa:bb:cc:dd:ee:02/meta")
                        .header(header::COOKIE, format!("{SESSION_COOKIE}={manager}"))
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from("description=freezer&offline_k="))
                        .unwrap(),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::SEE_OTHER);

            let scope = super::super::THRESHOLD_SCOPE_EYE.to_string();
            let on_h2 = ({
                let a0 = "h2".to_string();
                let a1 = scope.clone();
                state.db.run(move |s| s.device_meta_map(&a0, &a1))
            })
            .await
            .expect("device_meta");
            let on_h1 = ({
                let a0 = "h1".to_string();
                state.db.run(move |s| s.device_meta_map(&a0, &scope))
            })
            .await
            .expect("device_meta");
            assert!(
                on_h2.contains_key("AA:BB:CC:DD:EE:02"),
                "written under the owning Hub"
            );
            assert!(on_h1.is_empty(), "nothing under the first Hub in the map");
        }
    }
}
