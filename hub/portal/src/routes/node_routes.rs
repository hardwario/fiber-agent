//! `GET /nodes`, `GET /nodes/{node_key}`, `GET /api/v1/chart/node/{node_key}/{field}`,
//! the Node (LoRaWAN STICKER) threshold/config commands, and the Node
//! backfill routes (M5-G). Context/wiring contracts:
//! `templates/pages/{nodes,node_detail}.html` + `templates/partials/
//! {node_card,backfill_jobs,limits_editor}.html` header comments.
//!
//! Phase 1 is single-device (see `AppState`'s doc-comment): every handler
//! here resolves "the device" the same way `history_routes`/`hub_routes`
//! do — the first entry of the store's `PortalState.devices` `BTreeMap`.
//!
//! # Threshold precedence
//!
//! `NodeState::field_thresholds` is the firmware echo (present as a list ⇒
//! authoritative, mqtt.md `thresholds_authoritative`); `field_thresholds`
//! (DB, scope `node`) is what a manager last set locally. Both pages/routes
//! that need a merged view go through `render::merge_node_thresholds` —
//! see that function's doc comment for the exact precedence rule (state
//! bounds win, DB `enabled` wins, each source alone is kept as-is).
//!
//! # Backfill error rendering (documented ambiguity)
//!
//! `partials/backfill_jobs.html` — the form's `hx-target` — has **no error
//! slot** (its header comment: just the jobs table or "No backfill jobs
//! yet."). Rendering `partials/command_status.html` into that target
//! instead would `outerHTML`-replace the `#backfill-jobs-{node_key}`
//! element with the alert markup, permanently destroying the SSE swap
//! target for that Node (a real DOM corruption, not a cosmetic one).
//! Resolution: validation/overlap failures return the **non-2xx status
//! `BackfillError`'s own doc comment already prescribes** (422 window /
//! 409 overlap) with a small plain-text body; htmx does not swap
//! non-2xx responses into the target by default, so the jobs table is left
//! untouched rather than corrupted. The tradeoff: no visible error banner
//! with the current template shape — a future revision adding an error
//! slot (or an `hx-swap-oob` command-status fragment) would need to touch
//! `partials/backfill_jobs.html`, outside this package's owned files.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use minijinja::context;
use portal_storage::THRESHOLD_SCOPE_NODE;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::{CurrentUser, RequireAdmin, RequireManager, Role};
use crate::backfill::{self, BackfillError};
use crate::http::AppState;
use crate::perms;
use crate::render;

use super::command::Cmd;
use super::history_routes::resolve_range_parts;
use super::{render as render_page, tr};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/nodes", get(nodes_page).post(add_node))
        // Before `/nodes/{node_key}` only for the reader; axum ranks the
        // static segment first regardless.
        .route("/nodes/add", get(node_add_page))
        .route("/nodes/{node_key}", get(node_detail_index))
        .route("/nodes/{node_key}/{tab}", get(node_detail_page))
        .route(
            "/api/v1/chart/node/{node_key}/{field}",
            get(chart_json_node),
        )
        .route(
            "/nodes/{node_key}/thresholds/{field}",
            post(set_node_threshold),
        )
        .route(
            "/nodes/{node_key}/sensors/{field}",
            post(set_node_sensor_enabled),
        )
        .route("/nodes/{node_key}/config", post(set_node_config))
        .route("/nodes/{node_key}/meta", post(set_node_meta))
        .route(
            "/nodes/{node_key}/device-config",
            post(set_node_device_config),
        )
        .route(
            "/nodes/{node_key}/history",
            get(node_history_tab).post(set_node_history),
        )
        .route("/nodes/{node_key}/refresh-config", post(refresh_config))
        .route("/nodes/{node_key}/read-device", post(read_device))
        .route("/nodes/{node_key}/dev-status", post(dev_status))
        .route("/nodes/{node_key}/pending", get(pending_fragment))
        .route("/nodes/{node_key}/pending/send", post(send_pending))
        .route("/nodes/{node_key}/pending/discard", post(discard_pending))
        .route("/nodes/{node_key}/limits/resync", post(resync_limits))
        .route(
            "/nodes/{node_key}/service/force-send",
            post(service_force_send),
        )
        .route(
            "/nodes/{node_key}/service/force-send/watch",
            get(service_force_send_watch),
        )
        .route("/nodes/{node_key}/service/send-msg", post(service_send_msg))
        .route("/nodes/{node_key}/service/reboot", post(service_reboot))
        .route("/nodes/{node_key}/remove", post(remove_node))
        .route("/nodes/{node_key}/move", post(move_node))
        .route("/nodes/{node_key}/keys/reveal", post(reveal_node_key))
        .route(
            "/nodes/{node_key}/messages.json",
            get(download_node_messages),
        )
        .route("/nodes/{node_key}/backfill", post(request_backfill))
        .route(
            "/nodes/{node_key}/backfill/{job_id}/cancel",
            post(cancel_backfill),
        )
}

/// The Hub's hostname, but only if it actually knows this `node_key`.
///
/// `pending_config` is keyed `(hostname, node_key, param_key)`, exactly like
/// `device_meta` is keyed `(hostname, scope, entity)` — so a draft written
/// for a key the store has never seen plants a row that the *next* device
/// registered under that key inherits, and its detail page then shows an
/// unsent-configuration banner nobody drafted, with a Send button that
/// pushes a stale value to new hardware. `set_node_meta` has refused this
/// since §19/F7 for the same reason; the two draft routes did not.
///
/// Resolved through [`PortalState::hub_of_node`], so it is the Hub that
/// actually holds the key rather than the first Hub in the store
/// (devices.hub.md §12 E).
fn hostname_for_known_node(state: &AppState, node_key: &str) -> Option<String> {
    state
        .store
        .snapshot()
        .hub_of_node(node_key)
        .map(|(device, _)| device.hostname.clone())
}

/// The Hub that holds `node_key`, and the carrier the Node is on: this Hub's registry when it
/// holds the Node ([`crate::radio::RadioControl::carrier_for`], the one resolver), else the
/// `transport` its Hub last published — a Node of another Hub in the cluster, whose registry the
/// seam cannot read. `None` for a key no Hub knows, as [`hostname_for_known_node`].
fn hub_and_carrier(state: &AppState, node_key: &str) -> Option<(String, portal_domain::Transport)> {
    let snapshot = state.store.snapshot();
    let (device, node) = snapshot.hub_of_node(node_key)?;
    let at = state.radio.carrier_for(node_key);
    let carrier = if at.registered {
        at.carrier
    } else {
        node.transport
    };
    Some((device.hostname.clone(), carrier))
}

/// The Hub every Node is registered on: the Server (devices.node.md §5, dodatek 2026-10-13).
///
/// Nodes belong to the Server (decisions #8, #16, #40; devices.hub.md §1.2): it holds the
/// AppKeys, the registry and the P2P central, and a Client refuses `node-add` outright
/// (control.radio.md, amendment 2026-10-13). So registration names no Hub. It goes to the Hub
/// this portal runs on, the one [`crate::radio::RadioControl`] speaks for, whose `radio.mode`
/// decides the radio choice.
///
/// A portal whose own Hub is not in the store (a dev process, the mock) falls back to the one
/// Hub that is not a Client, when there is exactly one. Otherwise `None`: guessing among
/// several Hubs is how a registration used to land on a Client.
pub(crate) fn registration_server(state: &AppState) -> Option<String> {
    let snapshot = state.store.snapshot();
    if let Some(local) = snapshot
        .devices
        .values()
        .find(|d| super::hub_routes::is_local_device(&d.hostname, &state.local_hostname))
    {
        return Some(local.hostname.clone());
    }
    let mut servers = snapshot.devices.values().filter(|d| {
        state.cluster.get(&d.hostname).is_none() && !super::hub_routes::reports_client_role(d)
    });
    match (servers.next(), servers.next()) {
        (Some(only), None) => Some(only.hostname.clone()),
        _ => None,
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// Shared radio-threshold form (Node + Beacon — partials/limits_editor.html)
// ---------------------------------------------------------------------

/// `partials/limits_editor.html`'s 4-bound + `enabled` POST body,
/// shared by both Node and Beacon threshold commands (that partial's own
/// header: "Node (STICKER) and Beacon (EYE) field thresholds share the
/// exact same shape"). `enabled` is `Option<String>` rather than `bool`
/// because an unchecked checkbox submits NO `enabled` key at all — the
/// template's own contract: a missing key means `false`, not "unchanged".
#[derive(Deserialize)]
pub(crate) struct RadioThresholdForm {
    #[serde(default)]
    pub(crate) critical_low: String,
    #[serde(default)]
    pub(crate) warning_low: String,
    #[serde(default)]
    pub(crate) warning_high: String,
    #[serde(default)]
    pub(crate) critical_high: String,
    #[serde(default)]
    pub(crate) enabled: Option<String>,
    /// The Hystereze field (alarm_hysteresis.md §5). Absent = leave the stored value as it
    /// is (a client that predates the field); blank = back to the default.
    #[serde(default)]
    pub(crate) hysteresis: Option<String>,
}

/// `""` (after trimming) -> `None`; otherwise parses as `f64` or errors —
/// same "blank means unset, not zero" contract as
/// `hub_routes::parse_optional_float`, duplicated here for the
/// 4-bound (no alarm_low/high) radio-threshold form shape.
pub(crate) fn parse_optional_bound(raw: &str, field: &str) -> Result<Option<f64>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<f64>()
        .map(Some)
        .map_err(|_| tr("monitoring.shared.bound_must_be_number").replace("{field}", field))
}

/// The four nullable bounds of a field-threshold form, in
/// (critical_low, warning_low, warning_high, critical_high) order.
pub(crate) type ThresholdBounds = (Option<f64>, Option<f64>, Option<f64>, Option<f64>);

pub(crate) fn parse_radio_bounds(form: &RadioThresholdForm) -> Result<ThresholdBounds, String> {
    Ok((
        parse_optional_bound(&form.critical_low, "critical_low")?,
        parse_optional_bound(&form.warning_low, "warning_low")?,
        parse_optional_bound(&form.warning_high, "warning_high")?,
        parse_optional_bound(&form.critical_high, "critical_high")?,
    ))
}

/// The Hystereze field of a bounds form (alarm_hysteresis.md §5), checked against the bounds
/// it goes with, all in the same unit. `None` = the field was not posted (leave the stored
/// value), `Some(None)` = blank (back to the default), `Some(Some(h))` = an own value. The
/// hysteresis must be ≥ 0 and, when not 0, smaller than the narrowest gap between two
/// neighbouring set bounds: wider, a grade could never step down past the next bound.
pub(crate) fn parse_hysteresis(
    raw: Option<&str>,
    bounds: ThresholdBounds,
) -> Result<Option<Option<f64>>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Some(None));
    }
    let h = raw
        .replace(',', ".")
        .parse::<f64>()
        .ok()
        .filter(|h| h.is_finite() && *h >= 0.0)
        .ok_or_else(|| tr("monitoring.threshold.hysteresis_invalid"))?;
    let (cl, wl, wh, ch) = bounds;
    let set: Vec<f64> = [cl, wl, wh, ch].into_iter().flatten().collect();
    let gap = set
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|g| *g > 0.0)
        .fold(f64::INFINITY, f64::min);
    if h > 0.0 && h >= gap {
        return Err(
            tr("monitoring.threshold.hysteresis_too_wide").replace("{gap}", &format!("{gap}"))
        );
    }
    Ok(Some(Some(h)))
}

/// Stores one field's hysteresis as the form gave it ([`parse_hysteresis`]); `None` leaves it.
/// A row that follows the default and gets exactly the default back keeps following it, so
/// saving a form unchanged never pins today's default onto the field. Hysteresis is the
/// portal's alone (decision #41): it is written through, never drafted. `Ok(true)` = moved.
pub(crate) async fn save_hysteresis(
    state: &AppState,
    hostname: &str,
    scope: &'static str,
    entity: &str,
    field: &str,
    submitted: Option<Option<f64>>,
) -> Result<bool, ()> {
    let Some(submitted) = submitted else {
        return Ok(false);
    };
    let default = crate::threshold_defaults::hysteresis(
        &crate::threshold_defaults::configured(),
        scope,
        field,
        None,
    );
    let (hostname, entity, field) = (hostname.to_string(), entity.to_string(), field.to_string());
    state
        .db
        .write(move |s| {
            let own = s.field_threshold_hysteresis(&hostname, scope, &entity, &field)?;
            let value = match submitted {
                Some(h) if own.is_none() && (h - default).abs() < 1e-9 => None,
                other => other,
            };
            s.set_field_threshold_hysteresis(&hostname, scope, &entity, &field, value)
        })
        .await
        .map_err(|err| tracing::warn!(%err, "the hysteresis was not stored"))
}

/// The hysteresis one field is graded with now: its own, else its quantity's default
/// (`threshold_defaults::hysteresis`). What a command to the Hub carries (§7).
pub(crate) async fn effective_hysteresis(
    state: &AppState,
    hostname: &str,
    scope: &'static str,
    entity: &str,
    field: &str,
) -> f64 {
    let (h, e, f) = (hostname.to_string(), entity.to_string(), field.to_string());
    let own = state
        .db
        .run(move |s| s.field_threshold_hysteresis(&h, scope, &e, &f))
        .await
        .ok()
        .flatten();
    crate::threshold_defaults::hysteresis(
        &crate::threshold_defaults::configured(),
        scope,
        field,
        own,
    )
}

/// What [`draft_node_band`] did with one field's band.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BandSave {
    /// Draft rows written.
    pub drafted: usize,
    /// Draft rows put back to what the portal holds, or dropped as no longer drafted.
    pub reverted: usize,
    /// The portal's own part of the band moved in `field_thresholds`: the warning bounds, and
    /// the switch of a band with no critical bound.
    pub stored: bool,
}

/// Saves one field's band for a Node. Every Node band save goes through here (N33, night test),
/// so all of them save the same way. Validation is the caller's: `parse_radio_bounds` and
/// [`check_bound_order`].
///
/// Only what reaches the Node is drafted (devices.node.md, amendment 2026-10-13): the critical
/// bounds, which are the Node's alarm rule (`radio::alarm`), and the switch of a band that has
/// or gets a critical bound. Of those, only what differs from what the portal holds is written,
/// and a value put back drops its earlier row (N17). The warning bounds are graded by the
/// portal alone (decision #41), so like the hysteresis they are stored at once and never
/// drafted; so is the switch of a band with no critical bound, which nothing on the Node holds.
pub(crate) async fn draft_node_band(
    state: &AppState,
    author: &str,
    hostname: &str,
    node_key: &str,
    field: &str,
    bounds: ThresholdBounds,
    enabled: bool,
) -> Result<BandSave, ()> {
    // N17 (combined23): only what changes is drafted. The form posts all four bounds and the
    // switch, and a row per posted value listed "critical_low 5 -> 5.0" and "enabled true ->
    // true" as changes. A value back at what the portal holds drops its earlier draft row
    // instead; the send plan fills every untouched bound from the same merged view.
    let current = {
        let node = state
            .store
            .snapshot_device(hostname)
            .and_then(|d| d.nodes.get(node_key).cloned());
        merged_thresholds_for(state, hostname, node.as_ref(), node_key)
            .await
            .remove(field)
    };
    let held_critical = [
        current.as_ref().and_then(|c| c.critical_low),
        current.as_ref().and_then(|c| c.critical_high),
    ];
    let held_warnings = (
        current.as_ref().and_then(|c| c.warning_low),
        current.as_ref().and_then(|c| c.warning_high),
    );
    // A field the portal holds no bounds for goes out switched on (`pending::plan_send`), so
    // on is what it holds.
    let held_enabled = current.as_ref().is_none_or(|c| c.enabled);
    let same = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() < 1e-9,
        (None, None) => true,
        _ => false,
    };
    let rows = match state.pending.list(hostname, node_key).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(%err, node_key, field, "draft unreadable; the band is not saved");
            return Err(());
        }
    };
    let of_field =
        |key: &str| crate::pending::parse_threshold_param_key(key).filter(|(f, _)| f == field);

    // PF-3b review: while the Node holds a discarded band of this field (`differs`), the held
    // critical bounds are drafted, not dropped (N17's exception) — one Send then puts the
    // portal's band back on the Node, whether or not its own re-draft is still there.
    let differs = state.pending.field_differs(hostname, node_key, field).await;
    let (critical_low, warning_low, warning_high, critical_high) = bounds;
    // The switch concerns the Node while the band has, gets or is losing a critical bound:
    // the Node's rule carries it (amendment 2026-10-13). Otherwise it is the portal's alone.
    let critical_drafted = rows.iter().any(|r| {
        of_field(&r.param_key).is_some_and(|(_, b)| b == "critical_low" || b == "critical_high")
    });
    let node_switch = differs
        || critical_drafted
        || [critical_low, critical_high]
            .into_iter()
            .chain(held_critical)
            .any(|v| v.is_some());

    // (param_key, desired value as JSON) to draft, and (param_key, held value as JSON) for the
    // keys put back to what the portal holds.
    let mut writes: Vec<(String, String)> = Vec::new();
    let mut reverts: Vec<(String, String)> = Vec::new();
    // config_sync.md §13 Q20d: the portal stores the critical bounds at once (below), so what it
    // holds is no longer what the Node holds. A bound whose draft row already carries this value
    // is on its way and is left alone (no second downlink); a bound with a row is drafted
    // again; only a bound with no row is compared with what the portal holds.
    let drafted_value = |key: &str| {
        rows.iter()
            .find(|r| r.param_key == key)
            .map(|r| r.desired_value.clone())
    };
    for ((bound, value), held) in ["critical_low", "critical_high"]
        .into_iter()
        .zip([critical_low, critical_high])
        .zip(held_critical)
    {
        let key = crate::pending::threshold_param_key(field, bound);
        let as_json = serde_json::to_string(&value).unwrap_or_else(|_| "null".into());
        match drafted_value(&key) {
            Some(Some(raw))
                if serde_json::from_str::<Option<f64>>(&raw).is_ok_and(|v| same(v, value)) =>
            {
                continue;
            }
            Some(_) => {
                writes.push((key, as_json));
                continue;
            }
            None => {}
        }
        if same(value, held) && !differs {
            reverts.push((
                key,
                serde_json::to_string(&held).unwrap_or_else(|_| "null".into()),
            ));
        } else {
            writes.push((
                key,
                serde_json::to_string(&value).unwrap_or_else(|_| "null".into()),
            ));
        }
    }
    // `enabled` is drafted with the critical bounds (OQ3), not written through: toggling it
    // reaches the Node on Send — the walk's alarm unit carries it to the device and the drain's
    // mirror applies it Hub-side — and a Discard reverts it with the rest of the draft.
    let enabled_key = crate::pending::threshold_enabled_param_key(field);
    let enabled_drafted = drafted_value(&enabled_key)
        .map(|v| v.and_then(|raw| serde_json::from_str::<bool>(&raw).ok()));
    if node_switch && enabled_drafted == Some(Some(enabled)) {
        // On its way already (Q20d, as the bounds above).
    } else if node_switch && enabled_drafted.is_some() {
        writes.push((
            enabled_key.clone(),
            serde_json::to_string(&enabled).unwrap_or_else(|_| "true".into()),
        ));
    } else if node_switch {
        if enabled == held_enabled {
            reverts.push((enabled_key.clone(), held_enabled.to_string()));
        } else {
            writes.push((
                enabled_key.clone(),
                serde_json::to_string(&enabled).unwrap_or_else(|_| "true".into()),
            ));
        }
    }

    // The portal's part first, so the warnings are graded from this save on.
    let stored = store_portal_band(
        state,
        author,
        hostname,
        node_key,
        field,
        PortalBand {
            held_critical,
            held_warnings,
            held_enabled,
            critical: (critical_low, critical_high),
            warnings: (warning_low, warning_high),
            enabled: Some(enabled),
        },
    )
    .await?;

    let mut drafted = 0usize;
    for (param_key, desired) in writes {
        let row = portal_storage::NewPendingConfig {
            hostname: hostname.to_string(),
            node_key: node_key.to_string(),
            param_key,
            kind: portal_storage::PENDING_KIND_THRESHOLD.to_string(),
            desired_value: Some(desired),
            author: author.to_string(),
        };
        if let Err(err) = state.pending.upsert(row).await {
            tracing::warn!(%err, node_key, field, "failed to write the threshold draft");
            return Err(());
        }
        drafted += 1;
    }
    // PF-3 review, item 4: a row the Hub already took is not dropped by an edit back to the held
    // value — it takes that value back and is `undelivered` again, since the Node still lacks it.
    let mut reverted = 0usize;
    for (param_key, held) in reverts {
        match state
            .pending
            .revert_to_held(hostname, node_key, &param_key, &held)
            .await
        {
            Ok(outcome) => reverted += usize::from(outcome.is_some()),
            Err(err) => {
                tracing::warn!(%err, node_key, param_key, "failed to drop a reverted draft row");
                return Err(());
            }
        }
    }
    // What is no longer drafted at all: warning rows a draft from before 2026-10-13 still holds,
    // and the switch of a band the Node holds nothing of. Both were just stored.
    let stale = rows.iter().filter(|r| {
        (crate::pending::is_warning_key(&r.param_key) && of_field(&r.param_key).is_some())
            || (!node_switch && r.param_key == enabled_key)
    });
    for row in stale {
        match state
            .pending
            .remove(hostname, node_key, &row.param_key)
            .await
        {
            Ok(removed) => reverted += usize::from(removed),
            Err(err) => {
                tracing::warn!(%err, node_key, param_key = %row.param_key, "failed to drop a draft row no longer drafted");
                return Err(());
            }
        }
    }
    Ok(BandSave {
        drafted,
        reverted,
        stored,
    })
}

/// The part of a Node band the portal holds alone, for [`store_portal_band`].
struct PortalBand {
    /// The critical bounds, warning bounds and switch the portal holds now (the merged view).
    held_critical: [Option<f64>; 2],
    held_warnings: (Option<f64>, Option<f64>),
    held_enabled: bool,
    /// The critical bounds saved: stored and graded at once, before the Node confirms them
    /// (config_sync.md §13 Q20d); the draft carries them to the Node.
    critical: (Option<f64>, Option<f64>),
    /// The warning bounds saved.
    warnings: (Option<f64>, Option<f64>),
    /// The switch saved, when it is the portal's alone; `None` keeps the stored one.
    enabled: Option<bool>,
}

/// Writes the saved band — warning and critical bounds and the switch — straight into the
/// field's `field_thresholds` row, audited in its history like any write (decision #41,
/// devices.node.md amendment 2026-10-13, config_sync.md §13 Q20d: graded at once, whatever
/// the Node confirms). A field with no row gets one only when something differs from what the
/// portal holds. `Ok(true)` = the row moved.
async fn store_portal_band(
    state: &AppState,
    author: &str,
    hostname: &str,
    node_key: &str,
    field: &str,
    band: PortalBand,
) -> Result<bool, ()> {
    let same = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() < 1e-9,
        (None, None) => true,
        _ => false,
    };
    let unchanged = same(band.warnings.0, band.held_warnings.0)
        && same(band.warnings.1, band.held_warnings.1)
        && same(band.critical.0, band.held_critical[0])
        && same(band.critical.1, band.held_critical[1])
        && band.enabled.is_none_or(|e| e == band.held_enabled);
    let (h, n, f, author) = (
        hostname.to_string(),
        node_key.to_string(),
        field.to_string(),
        author.to_string(),
    );
    state
        .db
        .write(move |s| {
            let (h, n, f) = (h.clone(), n.clone(), f.clone());
            let row = match s.get_field_threshold(&h, THRESHOLD_SCOPE_NODE, &n, &f)? {
                Some(held) => portal_storage::NewFieldThreshold {
                    hostname: h,
                    scope: THRESHOLD_SCOPE_NODE.to_string(),
                    entity: n,
                    field: f,
                    critical_low: band.critical.0,
                    warning_low: band.warnings.0,
                    warning_high: band.warnings.1,
                    critical_high: band.critical.1,
                    enabled: band.enabled.unwrap_or(held.enabled),
                },
                None if unchanged => return Ok(false),
                None => portal_storage::NewFieldThreshold {
                    hostname: h,
                    scope: THRESHOLD_SCOPE_NODE.to_string(),
                    entity: n,
                    field: f,
                    critical_low: band.critical.0,
                    warning_low: band.warnings.0,
                    warning_high: band.warnings.1,
                    critical_high: band.critical.1,
                    enabled: band.enabled.unwrap_or(band.held_enabled),
                },
            };
            s.upsert_field_threshold(&row, Some(&author))
        })
        .await
        .map_err(|err| tracing::warn!(%err, node_key, field, "the warning bounds were not stored"))
}

/// Stores a switched-off band straight in `field_thresholds` when nothing about the field is
/// live: the portal holds it off (a stored row that is off, or no row), no draft row touches
/// the field, and the Node does not hold a discarded band of it (PF-3b). `true` = stored; on
/// `false` the caller drafts as usual, so a band that is on, or one on its way, is still
/// switched off on the Hub and the Node by a Send.
async fn store_switched_off_band(
    state: &AppState,
    hostname: &str,
    node_key: &str,
    field: &str,
    bounds: ThresholdBounds,
    author: &str,
) -> bool {
    let node = state
        .store
        .snapshot_device(hostname)
        .and_then(|d| d.nodes.get(node_key).cloned());
    let held_on = merged_thresholds_for(state, hostname, node.as_ref(), node_key)
        .await
        .get(field)
        .is_some_and(|t| t.enabled);
    if held_on || state.pending.field_differs(hostname, node_key, field).await {
        return false;
    }
    let drafted = match state.pending.list(hostname, node_key).await {
        Ok(rows) => rows.iter().any(|r| {
            crate::pending::parse_threshold_param_key(&r.param_key).is_some_and(|(f, _)| f == field)
                || crate::pending::parse_threshold_enabled_key(&r.param_key).as_deref()
                    == Some(field)
        }),
        Err(err) => {
            tracing::warn!(%err, node_key, "draft unreadable; the band is drafted instead");
            true
        }
    };
    if drafted {
        return false;
    }
    let (critical_low, warning_low, warning_high, critical_high) = bounds;
    let row = portal_storage::NewFieldThreshold {
        hostname: hostname.to_string(),
        scope: THRESHOLD_SCOPE_NODE.to_string(),
        entity: node_key.to_string(),
        field: field.to_string(),
        critical_low,
        warning_low,
        warning_high,
        critical_high,
        enabled: false,
    };
    let author = author.to_string();
    match state
        .db
        .write(move |s| s.upsert_field_threshold(&row, Some(&author)))
        .await
    {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(%err, node_key, field, "a switched-off band was not stored; drafting it");
            false
        }
    }
}

/// The bounds rise along the axis: critical low < warning low <= warning high < critical high,
/// wherever two of them are set (N22, night test). The form only clamps its drawing, so an
/// inverted band reached the draft: the Node's alarm table skipped it (no rule, nothing said),
/// and equal critical bounds went out as a rule the firmware refuses (`hi > lo`), failing the
/// whole send.
/// P-3 (nodes_phase2.cs.md, dodatek 2026-09-25): the Node's alarm rule is a range, so a
/// critical band on a field the Node watches itself is always both bounds. One left blank is
/// filled with a value that quantity never reaches, and stored like any bound, so the table,
/// the draft and the Node's rule all show the same band. Fields the Node does not watch keep a
/// one-sided band; the portal grades those itself.
pub(crate) fn complete_critical_band(field: &str, bounds: ThresholdBounds) -> ThresholdBounds {
    let (cl, wl, wh, ch) = bounds;
    let Some((open_low, open_high)) = crate::radio::alarm::open_band(field) else {
        return bounds;
    };
    match (cl, ch) {
        (Some(_), None) => (cl, wl, wh, Some(open_high)),
        (None, Some(_)) => (Some(open_low), wl, wh, ch),
        _ => bounds,
    }
}

pub(crate) fn check_bound_order(bounds: ThresholdBounds) -> Result<(), String> {
    let (cl, wl, wh, ch) = bounds;
    let below = |a: Option<f64>, b: Option<f64>, strict: bool| match (a, b) {
        (Some(a), Some(b)) => {
            if strict {
                a < b
            } else {
                a <= b
            }
        }
        _ => true,
    };
    let rising = below(cl, ch, true)
        && below(cl, wl, false)
        && below(wl, wh, false)
        && below(wh, ch, false)
        && below(cl, wh, false)
        && below(wl, ch, false);
    if rising {
        Ok(())
    } else {
        Err(tr("monitoring.shared.bounds_out_of_order"))
    }
}

// ---------------------------------------------------------------------
// GET /nodes
// ---------------------------------------------------------------------

/// How the Node list is ordered. Filtering is deliberately absent: the spec
/// renders every row and filters client-side (devices.node.md §2.9), which
/// is why §4 makes a 500-row list under live SSE churn a mandatory
/// acceptance gate rather than an optimisation note. Sorting cannot follow
/// it — the server decides what order rows arrive in, and an SSE row swap
/// replaces a row in place rather than re-inserting it, so the order has to
/// be right the first time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NodeSort {
    /// Registration time, newest first (devices.backend.md §4's default).
    #[default]
    CreatedAt,
    Name,
    LastSeen,
    /// The derived link-quality rung, worst first.
    Signal,
    AlarmState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SortDir {
    Asc,
    #[default]
    Desc,
}

/// The "Mimo meze" filter's vocabulary: the **three value states** the
/// 2026-09-01 (4) spec edit unified across devices.node.md §2.2,
/// devices.beacon.md §2.2 and monitoring.md §2.2. It replaces the earlier
/// five-way `Not set` / High–Low × Warning–Critical breakdown, which B1
/// was written against — with three states there is no longer a way to ask
/// for "no bounds configured", and that is deliberate: an unconfigured
/// field reads as `Normal` on the value axis, and "nothing is watching this
/// field" is a configuration question rather than a list filter.
///
/// The client evaluates it against `NodeFieldCardCtx.alarm_state` — the
/// per-field verdict already on every row — and a Node matches when **at
/// least one** of its fields is in the selected state (spec §2.2 point 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ThresholdFilter {
    Normal,
    Warning,
    Critical,
}

impl ThresholdFilter {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "normal" => Some(Self::Normal),
            "warning" => Some(Self::Warning),
            "critical" => Some(Self::Critical),
            _ => None,
        }
    }
}

/// Normalises `?threshold=` into the vocabulary above, **dropping** what it
/// does not recognise.
///
/// This is the one filter the server parses rather than passes through, and
/// the reason is the vocabulary change itself: a bookmark from before
/// 2026-09-01 carries `warning_high`, and handing that to the client
/// unexamined would preseed a control with a value that no longer exists —
/// a filter that silently matches nothing. Dropping it opens the list
/// unfiltered instead, which is the same failure mode every other field in
/// `NodeListQuery` already has. Duplicates collapse; order does not matter
/// (the client ORs the set).
fn parse_threshold_filter(raw: Option<&str>) -> Vec<ThresholdFilter> {
    let mut out: Vec<ThresholdFilter> = Vec::new();
    for token in raw.unwrap_or_default().split(',') {
        if let Some(parsed) = ThresholdFilter::parse(token) {
            if !out.contains(&parsed) {
                out.push(parsed);
            }
        }
    }
    out
}

/// `/nodes?...` — shareable, survives a reload and the full-refresh
/// fallback.
///
/// Every field is `Option` and unknown values fall back to the default
/// rather than 400: a stale bookmark or a hand-edited URL should render the
/// list, not an error page.
///
/// `sort` and `dir` are applied here, by `sort_nodes`. The filter fields
/// are carried through to the template untouched — by design the server
/// does not act on them, it hands the client its initial state — but the
/// client half is **deferred and unbuilt** (`devices.backend.md` §0), so
/// today a shared link's filters are dropped on the floor while its sort
/// survives. Recorded rather than papered over, because a half-honoured
/// query string is worse to debug than one nobody honours.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct NodeListQuery {
    #[serde(default)]
    pub sort: Option<NodeSort>,
    #[serde(default)]
    pub dir: Option<SortDir>,
    /// Fulltext needle (name, key, serial, location, description).
    #[serde(default)]
    pub q: Option<String>,
    /// Comma-separated gateway ids.
    #[serde(default)]
    pub gateway: Option<String>,
    /// Comma-separated value-class names.
    #[serde(default)]
    pub state: Option<String>,
    /// Comma-separated value states — `normal`, `warning`, `critical`
    /// (`ThresholdFilter`). Unlike its siblings this one is parsed rather
    /// than passed through; see `parse_threshold_filter`.
    #[serde(default)]
    pub threshold: Option<String>,
    /// `1` to show only Nodes on the operational-fault axis.
    #[serde(default)]
    pub fault: Option<String>,
    /// B4's registration/removal flash codes, the shape `beacons.html` has
    /// used since B0: a full-page POST + redirect, because registering a
    /// Node changes the list itself rather than one row of it.
    #[serde(default)]
    pub ok: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Orders the rendered rows. Ties break on `node_key`, which is unique and
/// stable — without it a re-render can shuffle equal rows and the list
/// appears to jitter under SSE churn.
fn sort_nodes(nodes: &mut [render::NodeCardCtx], sort: NodeSort, dir: SortDir) {
    nodes.sort_by(|a, b| {
        let ordering = match sort {
            // Absent metadata sorts last under the default (newest first),
            // because a Node with no registration row is not newer than one
            // that has one — it is simply unknown.
            NodeSort::CreatedAt => a.created_at.cmp(&b.created_at),
            NodeSort::Name => a
                .name
                .as_deref()
                .unwrap_or(&a.node_key)
                .to_lowercase()
                .cmp(&b.name.as_deref().unwrap_or(&b.node_key).to_lowercase()),
            NodeSort::LastSeen => a.last_seen.cmp(&b.last_seen),
            // Ascending rank is worst-first by construction (`Critical` is
            // rank 0). The `dir` reversal below is applied once, to this
            // like every other key — reversing here as well would silently
            // invert the column against its own arrow.
            NodeSort::Signal => a.signal.severity_rank().cmp(&b.signal.severity_rank()),
            NodeSort::AlarmState => a.alarm_state.cmp(&b.alarm_state),
        };
        let ordering = match dir {
            SortDir::Asc => ordering,
            SortDir::Desc => ordering.reverse(),
        };
        ordering.then_with(|| a.node_key.cmp(&b.node_key))
    });
}

async fn nodes_page(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Query(q): Query<NodeListQuery>,
) -> Response {
    nodes_page_with(&state, &current_user, q, None).await
}

/// `GET /nodes/add` — the registration form as a page, like `/beacons/add`
/// (devices.node.md, dodatek 2026-10-14). Same context as the list, which
/// computes the registration keys; a viewer is sent back to the list, and
/// `POST /nodes` stays the gate.
async fn node_add_page(current_user: CurrentUser, State(state): State<AppState>) -> Response {
    if !render::can_edit_thresholds(&current_user) {
        return Redirect::to("/nodes").into_response();
    }
    render_nodes(
        &state,
        &current_user,
        NodeListQuery::default(),
        None,
        "pages/node_add.html",
    )
    .await
}

/// The Nodes list, with `refusal` in the error flash when a registration the Hub refused
/// renders it (WP4: the refusal used to come back as the bare `#command-status` fragment, which
/// a plain form POST painted over the whole page).
async fn nodes_page_with(
    state: &AppState,
    current_user: &CurrentUser,
    q: NodeListQuery,
    refusal: Option<String>,
) -> Response {
    render_nodes(state, current_user, q, refusal, "pages/nodes.html").await
}

/// [`nodes_page_with`] onto `template`: the list or the add page.
async fn render_nodes(
    state: &AppState,
    current_user: &CurrentUser,
    q: NodeListQuery,
    refusal: Option<String>,
    template: &str,
) -> Response {
    let portal_state = state.store.snapshot();
    // Every Hub's Nodes in one list (devices.hub.md §12 E). The row carries
    // its own hostname, so the SSE target and the Hub column stay right in a
    // mixed list; `hostname` below is still the first Hub, for the empty
    // state's message and for nothing else.
    // The `tbody`'s `node-added-…` / `node-removed-…` names, so the canonical
    // `DeviceState::hostname` the rows are drawn with and the SSE hub names its
    // events after — not the store's lowercase key, a third spelling no event
    // carries (#87).
    let hostnames: Vec<String> = portal_state
        .devices
        .values()
        .map(|d| d.hostname.clone())
        .collect();
    let hostname = portal_state
        .devices
        .values()
        .next()
        .map(|d| d.hostname.clone());
    // Registration always goes to the Server (devices.node.md §5, dodatek 2026-10-13), so the
    // dialog names no Hub and everything it prefills comes from that one.
    let server = registration_server(state);
    // #86: the registration dialog's JoinEUI prefill — the Server's default the agent applies
    // to a blank field; blank when it has not reported one (no v2 radio, or an agent that
    // predates the field).
    let default_join_eui = server
        .as_deref()
        .and_then(|s| portal_state.device(s))
        .and_then(|d| d.lorawan.as_ref())
        .and_then(|l| l.join_eui.clone());

    let mut nodes: Vec<render::NodeCardCtx> = Vec::new();
    for device in portal_state.devices.values() {
        // One read set per Hub, not per Node — the N+1 rule `node_row_data`
        // exists for.
        let rows = node_row_data(state, &device.hostname).await;
        nodes.extend(device.nodes.values().map(|node| {
            let mut ctx = rows.card(node, device);
            ctx.hostname = Some(device.hostname.clone());
            ctx
        }));
    }

    let sort = q.sort.unwrap_or_default();
    let dir = q.dir.unwrap_or_default();
    sort_nodes(&mut nodes, sort, dir);
    let mode = state.radio.mode();

    // GitLab #109: registration is a signed command, so the Hubs this portal is not paired with
    // are named above the Register button, and the button (and the dialog's submit) is disabled
    // while the Server cannot sign one. Only the Server's pairing counts: registration goes
    // nowhere else (devices.node.md §5, dodatek 2026-10-13).
    let can_edit = render::can_edit_thresholds(current_user);
    let unpaired = super::unpaired_banner(state, current_user, &hostnames, "/nodes").await;
    let register_blocked = server.as_deref().is_some_and(|s| {
        unpaired
            .hubs
            .iter()
            .any(|h| h.hostname.eq_ignore_ascii_case(s))
    });

    let ctx = render::page_ctx(
        context! {
            nav_active => "nodes",
            hostname => hostname,
            hostnames => hostnames,
            nodes => nodes,
            // The seam for §4's filter controls, which are **not built
            // yet** — `devices.backend.md` §0 records "URL-query filters"
            // among the deferred items, and `nodes.html` reads none of
            // this. Kept, and kept accurate, because the vocabulary
            // normalisation (`parse_threshold_filter`) is the part that
            // rots against stale bookmarks and is already tested.
            //
            // What this means today: `sort` and `dir` ARE applied, by
            // `sort_nodes` above, so a shared `/nodes?sort=…` link does
            // open with the same order — but the four filter fields below
            // are carried, not acted on, by either side. A shared link
            // carrying `?q=` or `?threshold=` opens the full list.
            filters => context! {
                sort => sort,
                dir => dir,
                q => q.q,
                gateway => q.gateway,
                state => q.state,
                threshold => parse_threshold_filter(q.threshold.as_deref()),
                fault => q.fault,
            },
            // B4: registration/removal flash and the dialog's two closed
            // vocabularies. `can_edit` gates the Register button, matching
            // `POST /nodes`' own `RequireManager`.
            ok => q.ok.and_then(super::non_empty).map(|c| node_list_flash(&c)),
            error => refusal.or_else(|| q.error.and_then(super::non_empty).map(|c| node_list_flash(&c))),
            can_edit => can_edit,
            unpaired => unpaired,
            register_blocked => register_blocked,
            default_join_eui => default_join_eui,
            // The dialog's radio choice (devices.node.md, dodatek 2026-09-27): preselected from
            // the Server's `radio.mode` (P2P on `dual`), a carrier the Server has switched off
            // shown and disabled whatever the Clients run (dodatek 2026-10-13). The JoinEUI
            // field is LoRaWAN's: shown for that choice only, labelled so on a `dual` Server.
            radio_mode => mode.map(|m| m.as_str()),
            registration_radio => context! {
                default => crate::radio::registration_default(mode).as_str(),
                lorawan => crate::radio::carrier_enabled(mode, portal_domain::Transport::Lorawan),
                p2p => crate::radio::carrier_enabled(mode, portal_domain::Transport::P2p),
            },
        },
        Some(current_user),
    );
    render_page(state, template, ctx)
}

// ---------------------------------------------------------------------
// GET /nodes/{node_key}
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct RangeQuery {
    range: Option<String>,
    /// Which field the chart opens on. View state the page writes back
    /// into the URL itself (`history.replaceState`), so an SSE `refresh`
    /// reload comes back to the same view; the tab is in the path.
    metric: Option<String>,
    /// Flash codes from the metadata form's redirect (`?ok=` / `?error=`).
    /// A full-page POST + redirect, not htmx: the metadata write touches
    /// SQLite only, so there is no `CommandOutcome` to render into
    /// `#command-status` and nothing would be gained by synthesizing one.
    ok: Option<String>,
    error: Option<String>,
}

/// `/nodes/{key}` and the old `?tab=` links land on a tab's own path.
async fn node_detail_index(
    _current_user: CurrentUser,
    Path(node_key): Path<String>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Response {
    super::device_tab_redirect(
        &format!("/nodes/{}", node_key.to_lowercase()),
        raw.as_deref(),
    )
}

/// GET of the History tab: `/nodes/{key}/history` is also the history
/// setting's POST, and matchit ranks that literal above `{tab}`.
async fn node_history_tab(
    current_user: CurrentUser,
    state: State<AppState>,
    Path(node_key): Path<String>,
    q: Query<RangeQuery>,
) -> Response {
    node_detail_page(current_user, state, Path((node_key, "history".into())), q).await
}

async fn node_detail_page(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path((node_key, tab)): Path<(String, String)>,
    Query(q): Query<RangeQuery>,
) -> Response {
    // An unknown tab is a 404, as on the Hub's page.
    if !super::DEVICE_TABS.contains(&tab.as_str()) {
        return super::not_found(&state, Some(&current_user));
    }
    let node_key = node_key.to_lowercase();
    let portal_state = state.store.snapshot();
    // The owning Hub, not the first one in the store (devices.hub.md §12 E).
    let Some((device, node)) = portal_state.hub_of_node(&node_key) else {
        return super::not_found(&state, Some(&current_user));
    };

    // §21 review, item 8: `NodeState::last_seen` is in-memory only, so
    // after a restart a stored reading is the only evidence this Node has
    // ever joined.
    // The same set also decides which fields the page offers.
    let stored_fields = ({
        let a0 = device.hostname.clone();
        let a1 = node_key.clone();
        state.db.run(move |s| s.node_fields_with_readings(&a0, &a1))
    })
    .await
    .unwrap_or_else(|err| {
        tracing::warn!(%err, node_key, "stored-reading probe failed; treated as none");
        Default::default()
    });
    let mut n = render::node_detail_ctx(
        node,
        device.node_infos.get(&node_key),
        device.node_device_infos.get(&node_key),
        device.node_configs.get(&node_key),
        state
            .offline
            .liveness_snapshot()
            .verdict(crate::notify::offline::DeviceScope::Node, &node_key),
        &stored_fields,
    )
    .with_device_status(device.node_device_statuses.get(&node_key))
    .with_last_fault(
        last_fault_event(&state, &device.hostname, &node_key)
            .await
            .as_ref(),
    );

    let db_thresholds = ({
        let a0 = device.hostname.clone();
        let a1 = THRESHOLD_SCOPE_NODE.to_string();
        let a2 = node_key.clone();
        state
            .db
            .run(move |s| s.list_field_thresholds(&a0, &a1, &a2))
    })
    .await
    .unwrap_or_else(|err| {
        tracing::warn!(%err, node_key, "failed to load node thresholds");
        Vec::new()
    });
    // Displayed units (UX-038): the page, its forms and the draft banner
    // all show pressure in hPa.
    let thresholds =
        render::displayed_node_thresholds(node.field_thresholds.as_deref(), &db_thresholds);

    let config_ctx = render::node_config_ctx(device.node_configs.get(&node_key));

    // The last-10 ring and the alarm window, concurrently with each other.
    // Both degrade to empty rather than failing the page: a detail page
    // without its history is still the page an operator came for.
    let (messages, events) = tokio::join!(
        ({
            let a0 = device.hostname.clone();
            let a1 = node_key.clone();
            state
                .db
                .run(move |s| s.list_raw_messages(&a0, &a1, Some(50)))
        }),
        ({
            let a0 = device.hostname.clone();
            let a1 = node_key.clone();
            let a3 = i64::MAX;
            let a4 = Some(50);
            state
                .db
                .run(move |s| s.query_events(&a0, &a1, None, 0, a3, a4))
        }),
    );
    let raw_rows = messages.unwrap_or_else(|err| {
        tracing::warn!(%err, node_key, "failed to load node message ring");
        Vec::new()
    });
    let messages: Vec<_> = raw_rows.iter().map(render::node_message_ctx).collect();
    let last_alarm = render::last_alarm_ctx(&events.unwrap_or_else(|err| {
        tracing::warn!(%err, node_key, "failed to load node events");
        Vec::new()
    }));

    let jobs = state
        .backfill
        .jobs_for(&node_key, None)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, node_key, "failed to load backfill jobs");
            Vec::new()
        });
    let jobs: Vec<_> = jobs.iter().map(backfill::job_ctx).collect();

    // B3 (§6, closing paragraph): the holes the jobs above are there to
    // fill. Not joined with `jobs_for` — both go through the backfill
    // service's single `Storage` mutex, so concurrency would only queue
    // them differently. Degrades to an empty list on its own.
    let gaps = state
        .backfill
        .gaps_ctx(&state.store, &device.hostname, &node_key)
        .await;

    // The operator's own metadata: description, the offline-detection
    // override and the auto-backfill toggle. Degrades to "no row yet",
    // which is what the table's absence means anyway — every default.
    let meta = ({
        let a0 = device.hostname.clone();
        let a1 = THRESHOLD_SCOPE_NODE.to_string();
        let a2 = node_key.clone();
        state.db.run(move |s| s.get_device_meta(&a0, &a1, &a2))
    })
    .await
    .unwrap_or_else(|err| {
        tracing::warn!(%err, node_key, "failed to load node metadata");
        None
    });
    let meta = render::device_meta_ctx(meta.as_ref());

    let can_edit = render::can_edit_thresholds(&current_user);
    // Removal is `RequireAdmin` (plan/portal/mqtt/design.md §13 item 3,
    // ratified 2026-09-15): the button follows the route's own gate, not the
    // manager-level `can_edit`, so nobody is shown a control that 403s.
    let can_remove = current_user.has_role(Role::Admin);

    // B4's §3.5 draft. Built from the same merged thresholds and the same
    // config pages the two sections below render, so the banner's "on the
    // device" column and the tables cannot disagree.
    // #126 item 5: through the shared builder, so the page shows what Send would refuse and
    // the frame count just as the SSE frames do.
    let pending = pending_ctx_for(&state, &device.hostname, &node_key, can_edit).await;

    // PF-3: whether each band is on the Node too, and since when — beside the bound, which is
    // evaluated from telemetry whatever the Node holds.
    let on_node = {
        let (h, n) = (device.hostname.clone(), node_key.clone());
        let delivery = state
            .db
            .run(move |s| s.list_node_delivery(&h, &n))
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(%err, node_key, "the Node's alarm-table record is unreadable");
                Vec::new()
            });
        // config_sync.md §13 Q20e: "not synchronised" only while no message is pending.
        let nothing_pending = state
            .pending
            .list(&device.hostname, &node_key)
            .await
            .map(|rows| rows.is_empty())
            .unwrap_or(false);
        render::node_delivery_ctx(
            &thresholds,
            &node_alarm_layout(Some(device), &node_key),
            &delivery,
            nothing_pending,
        )
    };
    let node_unsynced = on_node.values().any(|d| d.unsynced.is_some());

    let selected_range = q.range.unwrap_or_else(|| "24h".to_string());
    let active_tab = super::DEVICE_TABS
        .into_iter()
        .find(|t| *t == tab)
        .unwrap_or("overview");
    // One registry, one carrier per Node (node_registry.md §6). The badge shows the registry's
    // carrier, which a move changes before its answer comes back; a Node the local registry does
    // not hold (another Hub's) keeps what its Hub published. `radio` feeds "Změnit rádio".
    let at = state.radio.carrier_for(&node_key);
    if at.registered {
        n.transport = at.carrier;
    }
    let radio = context! {
        carrier => n.transport.as_str(),
        target => n.transport.other().as_str(),
        target_enabled => crate::radio::carrier_enabled(state.radio.mode(), n.transport.other()),
    };
    // "Změnit rádio" is `RequireAdmin`, like removal.
    let can_move = current_user.has_role(Role::Admin);

    // `?ok=moved` is the move's own flash; any other `ok` is the metadata form's.
    let field_label = |field: &str| {
        let fallback = fiber_protocol::lorawan_field(field).map_or(field, |f| f.label);
        crate::i18n::field_label(crate::i18n::current_lang(), field, fallback)
    };
    let (move_ok, meta_ok) = match q.ok.and_then(super::non_empty).as_deref() {
        Some(ok) if sensor_switch_flash(ok, field_label).is_some() => {
            (sensor_switch_flash(ok, field_label), None)
        }
        Some("moved") => (
            Some(tr("monitoring.node_detail.moved_ok").replace(
                "{carrier}",
                &tr(&format!("monitoring.transport.{}", n.transport.as_str())),
            )),
            None,
        ),
        Some(_) => (None, Some(tr("monitoring.meta.saved"))),
        None => (None, None),
    };
    // The removal dialog's refusals land on the Overview tab, where the
    // metadata form (and its flash) is not drawn, so they get their own
    // alert under the remove button (UX-044).
    let (remove_error, meta_error) = match q.error.and_then(super::non_empty).as_deref() {
        Some(code @ ("confirm_mismatch" | "remove_failed")) => (Some(node_list_flash(code)), None),
        // Refused before anything was signed — an unpaired Hub above all (#109).
        Some(code) if super::command::refusal_message(code).is_some() => {
            (super::command::refusal_message(code), None)
        }
        Some("description_too_long") => {
            (None, Some(tr("monitoring.meta.err_description_too_long")))
        }
        Some("name_too_long") => (None, Some(tr("monitoring.meta.err_name_too_long"))),
        Some("invalid_offline_k") => (
            None,
            Some(
                tr("monitoring.shared.count_must_be_number")
                    .replace("{field}", &tr("monitoring.meta.offline_k_label")),
            ),
        ),
        Some(_) => (None, Some(tr("monitoring.meta.err_save_failed"))),
        None => (None, None),
    };

    // The Overview tab's calibration card (calibration.cs.md §2) — read only
    // for the tab that draws it.
    let cal = match super::calibration_routes::Target::node(&state, &node_key) {
        Some(target) if active_tab == "overview" => {
            Some(super::calibration_routes::card_ctx(&state, &target, &current_user).await)
        }
        _ => None,
    };

    // The Overview's Channels card (ui_components.md §2).
    let channels = if active_tab == "overview" {
        Some({
            let (times, raw_at) =
                super::channels::node_times(&state, &device.hostname, &node_key).await;
            super::channels::node_card(
                format!("channels-{node_key}"),
                &device.hostname,
                &n,
                &thresholds,
                &times,
                raw_at,
                can_edit,
                render::now_unix(),
            )
        })
    } else {
        None
    };

    // The Diagnostics tab's Alarm messages and Message history cards
    // (ui_components.md, amendment 2026-10-15): read-only, for that tab only.
    let diag = if active_tab == "diagnostics" {
        Some(node_diagnostics(&state, &device.hostname, &node_key, &raw_rows).await)
    } else {
        None
    };

    // "Device information" (metadata.cs.md §5) opens the Overview; its home
    // Hub is the Nodes list's Gateway column (`HomeGateways::of`).
    let device_info = (active_tab == "overview").then(|| {
        let serial = n
            .serial_number
            .clone()
            .map(|s| render::node_serial(minijinja::Value::from(s)))
            .filter(|s| !s.is_empty());
        let mut di = render::node_info_ctx(
            &n,
            serial,
            meta.description.clone(),
            home_gateways(&state).of(node),
            cal.as_ref(),
            can_edit,
        );
        di.ok = meta_ok.clone();
        di.error = meta_error.clone();
        di
    });
    let ctx = render::page_ctx(
        context! {
            nav_active => "nodes",
            di => device_info,
            hostname => device.hostname.clone(),
            // GitLab #109: its limits, configuration and removal are signed commands to this Hub.
            unpaired => super::unpaired_banner(
                &state,
                &current_user,
                std::slice::from_ref(&device.hostname),
                &format!("/nodes/{node_key}"),
            )
            .await,
            n => n,
            has_limits => render::any_bounded(thresholds.values()),
            thresholds => thresholds,
            on_node => on_node,
            node_unsynced => node_unsynced,
            config => config_ctx,
            history => history_ctx_for(&state, &device.hostname, &node_key).await,
            pending => pending,
            raw_templates => render::raw_message_templates(device.node_configs.get(&node_key)),
            jobs => jobs,
            gaps => gaps,
            // The backfill form opens on the last 24 hours, in the display
            // zone it is read back in (decisions.md #32).
            backfill_from => crate::display_tz::datetime_local(render::now_unix() - 24 * 3600),
            backfill_to => crate::display_tz::datetime_local(render::now_unix()),
            messages => messages,
            last_alarm => last_alarm,
            diag => diag,
            home_gateway => home_gateways(&state).of(node),
            can_edit => can_edit,
            can_remove => can_remove,
            selected_range => selected_range,
            selected_metric => q.metric,
            active_tab => active_tab,
            meta => meta,
            // The blank offline field's placeholder: what this Node inherits
            // when it has no override, same role `beacon_detail_page` gives
            // `offline_k_default`.
            offline_k_default => state.offline.node_offline_k(),
            meta_ok => meta_ok,
            // The metadata card's location combobox (metadata.cs.md).
            locations => super::location_suggestions(&state).await,
            meta_error => meta_error,
            remove_error => remove_error,
            // Its registry carrier is one this Hub's `radio.mode` has switched off
            // (node_registry.md §11): "rádio na tomto Hubu vypnuté", with "Změnit rádio" beside
            // it for an admin.
            carrier_off => state.radio.carrier_switched_off(&node_key).map(|c| c.as_str()),
            radio => radio,
            can_move => can_move,
            move_ok => move_ok,
            cal => cal,
            ch => channels,
            evc => super::probe_routes::events_card(&state, &current_user, &format!("node:{node_key}"), None).await,
        },
        Some(&current_user),
    );
    render_page(&state, "pages/node_detail.html", ctx)
}

// ---------------------------------------------------------------------
// GET /api/v1/chart/node/{node_key}/{field}
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct NodeChartQuery {
    range: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    agg: Option<String>,
}

/// Same `{ts, avg, min, max}` shape as the wired endpoint — shared outright
/// so both go through one bucketer (`history_routes::apply_agg`).
use super::history_routes::ChartPoint as NodeChartPoint;

/// Same 6-key shape as `history_routes::ThresholdsJson` — `alarm_low`/
/// `alarm_high` are always `null` (radio field thresholds have no alarm
/// bounds, only the 4-bound critical/warning range).
#[derive(Serialize)]
struct NodeThresholdsJson {
    critical_low: Option<f64>,
    warning_low: Option<f64>,
    warning_high: Option<f64>,
    critical_high: Option<f64>,
    alarm_low: Option<f64>,
    alarm_high: Option<f64>,
}

#[derive(Serialize)]
struct NodeChartResponse {
    hostname: String,
    node_key: String,
    field: String,
    granularity: &'static str,
    agg_effective: &'static str,
    points: Vec<NodeChartPoint>,
    thresholds: Option<NodeThresholdsJson>,
    sensor_name: Option<String>,
    unit: String,
}

/// Raw `node_field_readings` rows, no tiering (this work package's brief:
/// "no tiering — raw rows"); downsampled by simple stride (take every
/// `ceil(len/cap)`-th point) when over [`CHART_POINT_CAP`], documented
/// rather than a real LTTB/min-max decimation — good enough for a 2000-pt
/// budget at this fleet size, cheap, and trivially testable.
const CHART_POINT_CAP: usize = 2000;

fn downsample(points: Vec<NodeChartPoint>, cap: usize) -> Vec<NodeChartPoint> {
    if points.len() <= cap || cap == 0 {
        return points;
    }
    let stride = points.len().div_ceil(cap).max(1);
    points.into_iter().step_by(stride).collect()
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

/// The Node's newest stored crash record (GitLab #135); `None` on none or a failed read, which
/// only costs the Device card its Last fault row.
pub(crate) async fn last_fault_event(
    state: &AppState,
    hostname: &str,
    node_key: &str,
) -> Option<portal_storage::NodeEvent> {
    let (a0, a1) = (hostname.to_string(), node_key.to_string());
    state
        .db
        .run(move |s| {
            s.query_events(
                &a0,
                &a1,
                Some(render::FATAL_ERROR_EVENT),
                0,
                i64::MAX,
                Some(1),
            )
        })
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, node_key, "failed to load the node's last fault");
            Vec::new()
        })
        .into_iter()
        .next()
}

/// One Node quantity's stored `(ts, value)` rows for `[from, to)`, oldest first: a count from
/// `node_counter_readings`, anything else from `node_field_readings` (GitLab #134).
pub(crate) fn node_series(
    s: &portal_storage::Storage,
    hostname: &str,
    node_key: &str,
    field: &str,
    is_count: bool,
    from: i64,
    to: i64,
) -> portal_storage::Result<Vec<(i64, Option<f64>)>> {
    Ok(if is_count {
        s.query_counters(hostname, node_key, field, from, to)?
            .into_iter()
            .map(|r| (r.ts, r.value.map(|v| v as f64)))
            .collect()
    } else {
        s.query_field_readings(hostname, node_key, field, from, to)?
            .into_iter()
            .map(|r| (r.ts, r.value))
            .collect()
    })
}

async fn chart_json_node(
    _current_user: CurrentUser,
    State(state): State<AppState>,
    Path((node_key, field)): Path<(String, String)>,
    Query(q): Query<NodeChartQuery>,
) -> Response {
    let node_key = node_key.to_lowercase();

    let Some(f) = fiber_protocol::lorawan_field(&field) else {
        return json_error(StatusCode::NOT_FOUND, "unknown field");
    };
    if !f.chartable {
        return json_error(StatusCode::NOT_FOUND, "field is not chartable");
    }

    let portal_state = state.store.snapshot();
    // Keyed by the Node, so the Hub comes from the Node (devices.hub.md
    // §12 E) — an unknown key is a 404 rather than the first Hub's history.
    let Some((device, node)) = portal_state.hub_of_node(&node_key) else {
        return json_error(StatusCode::NOT_FOUND, "no device");
    };
    let hostname = device.hostname.clone();
    let sensor_name = node.name.clone();

    let now = now_unix();
    let (from, to) = match resolve_range_parts(q.range.as_deref(), q.from, q.to, now) {
        Ok(range) => range,
        Err(message) => return json_error(StatusCode::BAD_REQUEST, message),
    };

    // A count is stored in `node_counter_readings`, not with the measurements (GitLab #134).
    let is_count = f.kind == fiber_protocol::LorawanFieldKind::Counter;
    let readings = match ({
        let a0 = hostname.clone();
        let a1 = node_key.clone();
        let a2 = field.clone();
        state
            .db
            .run(move |s| node_series(s, &a0, &a1, &a2, is_count, from, to))
    })
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(%err, hostname, node_key, field, "node chart query failed");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "query failed");
        }
    };
    // A raw scalar reading has no min/max spread — avg == min == max, so
    // the chart JS's range-band series draws a zero-width band (harmless).
    // In the displayed unit (UX-038), as are the thresholds and `unit` below.
    let shown = |v: Option<f64>| v.map(|v| f.to_display(v));
    let points: Vec<NodeChartPoint> = readings
        .iter()
        .map(|&(ts, value)| NodeChartPoint {
            ts,
            avg: shown(value),
            min: shown(value),
            max: shown(value),
        })
        .collect();
    // `agg=auto` (the default, and what the node detail page sends) keeps
    // the stride downsample this endpoint has always used; an explicit
    // bucket goes through the shared bucketer instead.
    let agg = super::history_routes::Agg::parse(q.agg.as_deref());
    let (points, agg_effective) = if agg == super::history_routes::Agg::Auto {
        (downsample(points, CHART_POINT_CAP), "raw")
    } else {
        super::history_routes::apply_agg(points, agg, 1, to - from)
    };

    let db_thresholds = ({
        let a0 = hostname.clone();
        let a1 = THRESHOLD_SCOPE_NODE.to_string();
        let a2 = node_key.clone();
        state
            .db
            .run(move |s| s.list_field_thresholds(&a0, &a1, &a2))
    })
    .await
    .unwrap_or_default();
    let state_thresholds = node.field_thresholds.as_deref();
    let merged = render::displayed_node_thresholds(state_thresholds, &db_thresholds);
    let thresholds = merged.get(&field).map(|t| NodeThresholdsJson {
        critical_low: t.critical_low,
        warning_low: t.warning_low,
        warning_high: t.warning_high,
        critical_high: t.critical_high,
        alarm_low: None,
        alarm_high: None,
    });

    Json(NodeChartResponse {
        hostname,
        node_key,
        field: field.clone(),
        granularity: "raw",
        agg_effective,
        points,
        thresholds,
        sensor_name,
        unit: f.display_unit().unwrap_or("").to_string(),
    })
    .into_response()
}

// ---------------------------------------------------------------------
// POST /nodes/{node_key}/thresholds/{field}
// ---------------------------------------------------------------------

/// Validates one editable config value and writes it into the Node's draft; shared by
/// `POST /nodes/{key}/config` and the Device configuration card. `Err` is the message to
/// show; nothing was drafted then.
async fn draft_config(
    state: &AppState,
    current_user: &CurrentUser,
    hostname: &str,
    node_key: &str,
    key: &str,
    value: &str,
) -> Result<(), String> {
    struct F<'a> {
        key: String,
        value: &'a str,
    }
    let form = F {
        key: key.to_string(),
        value,
    };
    let (hostname, node_key) = (hostname.to_string(), node_key.to_string());
    // Only the intervals are edited here (nodes_phase1.cs.md WP5); the service card's raw
    // message reaches the other writable keys. The range is the firmware's own, the one it
    // would answer OUT_OF_RANGE to, so a value outside it never reaches the draft.
    let Some(input) = render::config_input(&form.key) else {
        return Err(tr("monitoring.node_detail.config_not_editable"));
    };
    // A GP pin enum (v2.0.0) takes one of its shell names; an interval its range.
    let in_range = if input.options.is_empty() {
        form.value
            .trim()
            .parse::<i64>()
            .ok()
            .zip(control_radio::decode::node_params::node_param(&form.key))
            .is_some_and(|(value, param)| param.accepts(value))
    } else {
        input.options.iter().any(|o| o == form.value.trim())
    };
    if !in_range && !input.options.is_empty() {
        return Err(tr("monitoring.node_detail.config_range_error")
            .replace("{range}", &input.options.join(", ")));
    }
    if !in_range {
        return Err(tr("monitoring.node_detail.config_range_error")
            .replace("{range}", input.range.as_deref().unwrap_or("?")));
    }

    let coerced = coerce_config_value(&form.key, form.value);
    let mut config = serde_json::Map::new();
    config.insert(form.key.clone(), coerced.clone());

    if let Err(errors) = fiber_protocol::validate_sticker_config(&config) {
        let message = errors
            .into_iter()
            .map(|e| format!("{}: {}", e.key, e.reason))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(message);
    }

    let row = portal_storage::NewPendingConfig {
        hostname: hostname.clone(),
        node_key: node_key.clone(),
        param_key: form.key.clone(),
        kind: portal_storage::PENDING_KIND_CONFIG.to_string(),
        desired_value: Some(serde_json::to_string(&coerced).unwrap_or_else(|_| "null".into())),
        author: current_user.username.clone(),
    };
    if let Err(err) = state.pending.upsert(row).await {
        tracing::warn!(%err, node_key, key = %form.key, "failed to write the config draft");
        return Err(tr("monitoring.pending.err_draft_failed"));
    }

    super::audit_action(
        state,
        current_user,
        "node_config_draft",
        &node_key,
        json!({ "key": form.key, "value": form.value }),
        true,
    )
    .await;

    Ok(())
}

/// **A draft write since B4** (devices.node.md §3.5).
///
/// This used to validate and then relay `set_lorawan_field_threshold`
/// immediately. It now validates exactly as before and writes the four
/// bounds into `pending_config` instead; nothing leaves the portal until
/// somebody presses Send. The validation is deliberately unchanged and
/// deliberately still here — a draft that holds a value the wire will
/// reject is a failure the operator only discovers minutes later, at the
/// worst possible moment.
///
/// One row per **bound**, not one per field: the banner shows what was
/// changed, and "critical_high 8 -> 9.5" is what was changed. The send walk
/// regroups them and fills the untouched bounds from the merged current
/// thresholds (`pending::plan_send`).
///
/// `enabled` is **not** drafted. It is a portal-side switch over whether a
/// stored threshold is evaluated at all, not a device setting, so it keeps
/// writing through immediately — deferring it behind a Send would mean a
/// manager could not silence a noisy threshold without also pushing every
/// other pending change.
///
/// *Amended 2026-09-28 (threshold_defaults.md D5):* since OQ3 `enabled` is drafted with the
/// bounds (`draft_node_band`). The one save that writes through is a band saved switched off
/// on a field whose band is off already and has no draft ([`store_switched_off_band`]): a
/// default band being edited, say. Nothing about it is evaluated or held live, so there is
/// nothing to send.
///
/// *Amended 2026-10-13 (devices.node.md):* only what reaches the Node is drafted — the critical
/// bounds and the switch of a band with a critical bound. The warning bounds, and the switch of
/// a band without one, are stored at once like the hysteresis (`draft_node_band`).
async fn set_node_threshold(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path((node_key, field)): Path<(String, String)>,
    Form(form): Form<RadioThresholdForm>,
) -> Response {
    let node_key = node_key.to_lowercase();
    const COMMAND: &str = "set_lorawan_field_threshold";
    let cmd = Cmd::new(&state, &current_user, COMMAND);

    // The `no_device_yet` early return comes FIRST in every B4 handler:
    // `MUTATING_ROUTES`' role sweep POSTs to each of these as a manager and
    // as a viewer, and anything that reaches the relay before answering
    // makes that sweep wait out the relay's 20 s timeout per route.
    if state.store.snapshot().devices.is_empty() {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    }
    // A draft is only meaningful for a Node the Hub actually knows — see
    // `hostname_for_known_node` for what an orphan row does to the next
    // device registered under this key. It is also what says *which* Hub
    // this command goes to (devices.hub.md §12 E).
    let Some(hostname) = hostname_for_known_node(&state, &node_key) else {
        return cmd.fail(&tr("monitoring.shared.unknown_node"));
    };

    let Some(f) = fiber_protocol::lorawan_field(&field) else {
        return cmd.fail(&tr("monitoring.shared.unknown_field").replace("{field}", &field));
    };
    // The battery takes no band: the Node raises its own low-battery alarm
    // (threshold_defaults.md §5). Said as such, not as a bare "not thresholdable".
    if crate::threshold_defaults::is_battery(&field) {
        return cmd.fail(&tr("monitoring.shared.battery_not_thresholdable"));
    }
    if !f.thresholdable {
        return cmd
            .fail(&tr("monitoring.shared.field_not_thresholdable").replace("{field}", &field));
    }
    // Bounds only for a quantity the Node measures (devices.node.md §3.4, dodatek 2026-09-24).
    let active = {
        let snapshot = state.store.snapshot();
        let device = snapshot.device(&hostname);
        device
            .and_then(|d| d.nodes.get(&node_key))
            .map(|n| {
                render::active_node_fields(device.and_then(|d| d.node_configs.get(&node_key)), n)
                    .iter()
                    .any(|a| a.name == field)
            })
            .unwrap_or(false)
    };
    if !active {
        return cmd.fail(&tr("monitoring.shared.field_not_active").replace("{field}", &field));
    }
    if state
        .store
        .sensor_disabled(&hostname, THRESHOLD_SCOPE_NODE, &node_key, &field)
    {
        return cmd.fail(&tr("monitoring.sensor_switch.err_off"));
    }

    // The form is in the displayed unit (UX-038: hPa); the draft, the wire
    // and the device are in the registry's own unit (Pa).
    let (critical_low, warning_low, warning_high, critical_high) = match parse_radio_bounds(&form) {
        Ok((cl, wl, wh, ch)) => {
            let wire = |v: Option<f64>| v.map(|v| f.from_display(v));
            complete_critical_band(&field, (wire(cl), wire(wl), wire(wh), wire(ch)))
        }
        Err(msg) => return cmd.fail(&msg),
    };
    if let Err(msg) = check_bound_order((critical_low, warning_low, warning_high, critical_high)) {
        return cmd.fail(&msg);
    }
    // The hysteresis is a width in the displayed unit; the wire's is the same width scaled.
    let hysteresis = match parse_hysteresis(
        form.hysteresis.as_deref(),
        (critical_low, warning_low, warning_high, critical_high),
    ) {
        Ok(h) => h.map(|h| h.map(|h| f.from_display(h) - f.from_display(0.0))),
        Err(msg) => return cmd.fail(&msg),
    };
    let enabled = form.enabled.is_some();
    // Written through before the bounds are drafted (decision #41: the portal's alone).
    let hysteresis_moved = save_hysteresis(
        &state,
        &hostname,
        THRESHOLD_SCOPE_NODE,
        &node_key,
        &field,
        hysteresis,
    )
    .await
    .unwrap_or(false);

    // threshold_defaults.md D5: a band saved switched off on a field whose band is off — a
    // default band above all, or no band — is the portal's alone, like the default itself.
    // Nothing is evaluated, so nothing is drafted for the Hub or the Node; the bounds wait in
    // `field_thresholds` for the day it is switched on, which then drafts as usual.
    if !enabled
        && store_switched_off_band(
            &state,
            &hostname,
            &node_key,
            &field,
            (critical_low, warning_low, warning_high, critical_high),
            &current_user.username,
        )
        .await
    {
        super::audit_action(
            &state,
            &current_user,
            "node_threshold_set",
            &node_key,
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
        state.pending.nudge(&hostname, &node_key);
        return cmd.ok(&tr("monitoring.threshold.saved_off"));
    }

    let Ok(BandSave {
        drafted,
        reverted,
        stored,
    }) = draft_node_band(
        &state,
        &current_user.username,
        &hostname,
        &node_key,
        &field,
        (critical_low, warning_low, warning_high, critical_high),
        enabled,
    )
    .await
    else {
        return cmd.fail(&tr("monitoring.pending.err_draft_failed"));
    };

    super::audit_action(
        &state,
        &current_user,
        "node_threshold_draft",
        &node_key,
        json!({
            "field": field,
            "critical_low": critical_low,
            "warning_low": warning_low,
            "warning_high": warning_high,
            "critical_high": critical_high,
            "enabled": enabled,
            "drafted_bounds": drafted,
            "reverted_bounds": reverted,
            "stored_warnings": stored,
        }),
        true,
    )
    .await;

    state.pending.nudge(&hostname, &node_key);
    if drafted > 0 || reverted > 0 {
        send_after_save(&state, &current_user, &hostname, &node_key).await;
    }
    if drafted == 0 && reverted == 0 {
        if stored {
            return cmd.ok(&tr("monitoring.threshold.warnings_saved"));
        }
        if hysteresis_moved {
            return cmd.ok(&tr("monitoring.threshold.hysteresis_saved"));
        }
        return cmd.ok(&tr("monitoring.pending.nothing_changed"));
    }
    cmd.ok(&tr("monitoring.pending.saved_sending"))
}

// ---------------------------------------------------------------------
// POST /nodes/{node_key}/sensors/{field}
// ---------------------------------------------------------------------

/// The "Čidlo zapnuto" switch under a sensor's Upravit (`partials/sensor_switch.html`):
/// `enabled=false` switches the sensor off, anything else on.
#[derive(Deserialize)]
pub(crate) struct SensorSwitchForm {
    #[serde(default)]
    pub(crate) enabled: String,
}

impl SensorSwitchForm {
    pub(crate) fn switch_on(&self) -> bool {
        self.enabled != "false"
    }
}

/// `HX-Redirect` back to a device page's Přehled: the whole table changes (the row, its
/// bounds, the draft banner), so it is reloaded rather than patched.
pub(crate) fn sensor_switch_redirect(target: &str) -> Response {
    let mut response = StatusCode::OK.into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(target) {
        response.headers_mut().insert("HX-Redirect", value);
    }
    response
}

/// The flash a device page shows after [`sensor_switch_redirect`]: `ok` is
/// `sensor_on.{field}`, `sensor_off.{field}` or `sensor_off_send.{field}`. `None` for any
/// other `ok`, which belongs to the page's other forms.
pub(crate) fn sensor_switch_flash(ok: &str, label_of: impl Fn(&str) -> String) -> Option<String> {
    let (what, field) = ok.split_once('.')?;
    let key = match what {
        "sensor_on" => "monitoring.sensor_switch.enabled",
        "sensor_off" => "monitoring.sensor_switch.disabled",
        "sensor_off_send" => "monitoring.sensor_switch.disabled_send",
        _ => return None,
    };
    Some(tr(key).replace("{sensor}", &label_of(field)))
}

/// Switches one Node sensor off or on (GitLab #120, devices.node.md, amendment 2026-10-13).
///
/// Off: the sensor's stored history, alarms and bound history are deleted and nothing of it is
/// stored or graded from then on. Its bound goes too — straight from `field_thresholds` when
/// nothing holds it but the portal, otherwise as a draft clearing it, so the next Send takes it
/// off the Hub and the Node's alarm table (the Node holds the whole table, decision #23). On:
/// the sensor is stored again from its next value, with no bound.
async fn set_node_sensor_enabled(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path((node_key, field)): Path<(String, String)>,
    Form(form): Form<SensorSwitchForm>,
) -> Response {
    let node_key = node_key.to_lowercase();
    let enable = form.switch_on();
    let cmd = Cmd::new(
        &state,
        &current_user,
        if enable {
            "sensor_enable"
        } else {
            "sensor_disable"
        },
    );

    // First, as in every B4 handler: the MUTATING_ROUTES sweep posts here with no Hub.
    if state.store.snapshot().devices.is_empty() {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    }
    let Some(hostname) = hostname_for_known_node(&state, &node_key) else {
        return cmd.fail(&tr("monitoring.shared.unknown_node"));
    };
    let Some(f) = fiber_protocol::lorawan_field(&field) else {
        return cmd.fail(&tr("monitoring.shared.unknown_field").replace("{field}", &field));
    };
    let label = f.label.to_string();
    let shown = crate::i18n::field_label(crate::i18n::current_lang(), &field, f.label);
    // Only a quantity this Node measures; a switched-off one still counts.
    let active = {
        let snapshot = state.store.snapshot();
        let device = snapshot.device(&hostname);
        device
            .and_then(|d| d.nodes.get(&node_key))
            .map(|n| {
                render::active_node_fields(device.and_then(|d| d.node_configs.get(&node_key)), n)
                    .iter()
                    .any(|a| a.name == field)
            })
            .unwrap_or(false)
    };
    if !active {
        return cmd.fail(&tr("monitoring.shared.field_not_active").replace("{field}", &field));
    }

    let redirect = format!("/nodes/{node_key}/overview");
    if enable {
        let (h, n, fl) = (hostname.clone(), node_key.clone(), field.clone());
        let switched = state
            .db
            .write(move |s| s.enable_sensor(&h, THRESHOLD_SCOPE_NODE, &n, &fl))
            .await;
        let Ok(switched) = switched else {
            return cmd.fail(&tr("monitoring.sensor_switch.err_failed"));
        };
        state
            .store
            .set_sensor_disabled(&hostname, THRESHOLD_SCOPE_NODE, &node_key, &field, false);
        if redraft_history_channels(&state, &current_user, &hostname, &node_key).await {
            state.pending.nudge(&hostname, &node_key);
            send_after_save(&state, &current_user, &hostname, &node_key).await;
        }
        if !switched {
            return cmd.ok(&tr("monitoring.sensor_switch.already_on").replace("{sensor}", &shown));
        }
        super::audit_action(
            &state,
            &current_user,
            "sensor_enable",
            &node_key,
            json!({ "field": field }),
            true,
        )
        .await;
        return sensor_switch_redirect(&format!("{redirect}?ok=sensor_on.{field}"));
    }

    // The bound first: once the sensor is off its row no longer shows, so a draft that fails
    // here must stop the switch rather than leave a band nobody can see.
    let held = {
        let node = state
            .store
            .snapshot_device(&hostname)
            .and_then(|d| d.nodes.get(&node_key).cloned());
        merged_thresholds_for(&state, &hostname, node.as_ref(), &node_key)
            .await
            .remove(&field)
    };
    let drafted = state
        .pending
        .list(&hostname, &node_key)
        .await
        .map(|rows| {
            rows.iter().any(|r| {
                crate::pending::parse_threshold_param_key(&r.param_key)
                    .is_some_and(|(f, _)| f == field)
                    || crate::pending::parse_threshold_enabled_key(&r.param_key).as_deref()
                        == Some(field.as_str())
            })
        })
        .unwrap_or(true);
    let live = held.as_ref().is_some_and(|t| t.enabled)
        || drafted
        || state
            .pending
            .field_differs(&hostname, &node_key, &field)
            .await;
    let mut send = false;
    if live {
        // Bounds cleared and the band off where the portal holds a critical band; where it
        // holds none, the switch stays at its held "on", so a band that was only drafted leaves
        // no draft behind. (Since 2026-10-13 a drafted band's warnings are already stored, so
        // a held row alone no longer means the Node holds anything.)
        let held_critical = held
            .as_ref()
            .is_some_and(|t| t.critical_low.is_some() || t.critical_high.is_some());
        match draft_node_band(
            &state,
            &current_user.username,
            &hostname,
            &node_key,
            &field,
            (None, None, None, None),
            !held_critical,
        )
        .await
        {
            Ok(save) => send = save.drafted > 0,
            Err(()) => return cmd.fail(&tr("monitoring.pending.err_draft_failed")),
        }
        // A band with no critical bound is the portal's alone (amendment 2026-10-13): the save
        // above stored it cleared and off rather than drafting it, so the row just goes.
        let left = state
            .pending
            .list(&hostname, &node_key)
            .await
            .map(|rows| {
                rows.iter().any(|r| {
                    crate::pending::parse_threshold_param_key(&r.param_key)
                        .is_some_and(|(f, _)| f == field)
                        || crate::pending::parse_threshold_enabled_key(&r.param_key).as_deref()
                            == Some(field.as_str())
                })
            })
            .unwrap_or(true);
        if !send && !left {
            let (h, n, fl) = (hostname.clone(), node_key.clone(), field.clone());
            if let Err(err) = state
                .db
                .write(move |s| s.delete_field_threshold(&h, THRESHOLD_SCOPE_NODE, &n, &fl))
                .await
            {
                tracing::warn!(%err, node_key, field, "switched-off sensor kept its stored band");
            }
        }
    } else if held.is_some() {
        // Off, undrafted and in step with the Node: the portal's alone.
        let (h, n, fl) = (hostname.clone(), node_key.clone(), field.clone());
        if let Err(err) = state
            .db
            .write(move |s| s.delete_field_threshold(&h, THRESHOLD_SCOPE_NODE, &n, &fl))
            .await
        {
            tracing::warn!(%err, node_key, field, "switched-off sensor kept its stored band");
        }
    }

    let purge = {
        let (h, n, fl) = (hostname.clone(), node_key.clone(), field.clone());
        let sensor_id = format!("{}{label}", portal_domain::node_sensor_prefix(&node_key));
        let actor = current_user.username.clone();
        state
            .db
            .write(move |s| s.disable_sensor(&h, THRESHOLD_SCOPE_NODE, &n, &fl, &sensor_id, &actor))
            .await
    };
    let purge = match purge {
        Ok(purge) => purge,
        Err(err) => {
            tracing::warn!(%err, node_key, field, "sensor not switched off");
            return cmd.fail(&tr("monitoring.sensor_switch.err_failed"));
        }
    };
    state
        .store
        .set_sensor_disabled(&hostname, THRESHOLD_SCOPE_NODE, &node_key, &field, true);
    send |= redraft_history_channels(&state, &current_user, &hostname, &node_key).await;
    state.pending.nudge(&hostname, &node_key);
    let Some(purge) = purge else {
        return cmd.ok(&tr("monitoring.sensor_switch.already_off").replace("{sensor}", &shown));
    };
    super::audit_action(
        &state,
        &current_user,
        "sensor_disable",
        &node_key,
        json!({
            "field": field,
            "readings": purge.readings,
            "alarms": purge.alarms,
            "threshold_history": purge.threshold_history,
            "group_members": purge.group_members,
            "drafted": send,
        }),
        true,
    )
    .await;
    if send {
        send_after_save(&state, &current_user, &hostname, &node_key).await;
    }
    let ok = if send {
        "sensor_off_send"
    } else {
        "sensor_off"
    };
    sensor_switch_redirect(&format!("{redirect}?ok={ok}.{field}"))
}

// ---------------------------------------------------------------------
// POST /nodes/{node_key}/meta
// ---------------------------------------------------------------------

/// The Node half of `device_meta` (devices.backend.md §3, WP0.2).
///
/// `offline_k` is text rather than a number so that blank survives the round
/// trip: it means "inherit `nodes.offline_detector`", while `0` means
/// "offline detection off for this Node". The automatic history download is not on this
/// form any more: it is the Konfigurace card's History switch (`set_node_history`,
/// devices.node.md §3.9 amendment 2026-09-25), and a save here keeps it as it is.
#[derive(Deserialize)]
struct NodeMetaForm {
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

async fn set_node_meta(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
    Form(form): Form<NodeMetaForm>,
) -> Response {
    let node_key = node_key.to_lowercase();
    let back = match form.tab.as_deref() {
        Some("overview") => "overview",
        _ => "settings",
    };
    let detail = format!("/nodes/{node_key}/{back}");

    let portal_state = state.store.snapshot();
    if portal_state.devices.is_empty() {
        return Redirect::to(&format!("{detail}?error=no_device")).into_response();
    }
    // 404 for a key the store does not know (§19, F7) — the same rule the
    // Beacon meta POST follows, and the same reason: `device_meta` is keyed
    // by `(hostname, scope, entity)`, so writing one for a device that does
    // not exist plants a row the next device with that key inherits. The
    // hostname is the owning Hub's (devices.hub.md §12 E).
    let Some((device, _)) = portal_state.hub_of_node(&node_key) else {
        return not_found();
    };
    let hostname = device.hostname.clone();
    drop(portal_state);

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
        THRESHOLD_SCOPE_NODE,
        &node_key,
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
                .set_meta_label(&hostname, THRESHOLD_SCOPE_NODE, &node_key, n, l);
        }
    }
    if let Err(err) = &saved {
        tracing::warn!(%err, node_key, "failed to save node metadata");
    }

    super::audit_action(
        &state,
        &current_user,
        "node_meta_set",
        &node_key,
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
// POST /nodes/{node_key}/history
// ---------------------------------------------------------------------

/// The History switch's state for one Node: its config info, the draft and `device_meta`.
/// Shared by the detail page, the switch's own POST and the SSE repaints, so the card cannot
/// say different things depending on which of them drew it.
pub(crate) async fn history_ctx_for(
    state: &AppState,
    hostname: &str,
    node_key: &str,
) -> render::NodeHistoryCtx {
    let config = state
        .store
        .snapshot_device(hostname)
        .and_then(|d| d.node_configs.get(node_key).cloned());
    let drafted = state
        .pending
        .list(hostname, node_key)
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|r| r.param_key == render::HISTORY_ENABLE_KEY)
        .and_then(|r| r.desired_value)
        .and_then(|v| serde_json::from_str::<bool>(&v).ok());
    let meta = {
        let (a0, a1, a2) = (
            hostname.to_string(),
            THRESHOLD_SCOPE_NODE.to_string(),
            node_key.to_string(),
        );
        state
            .db
            .run(move |s| s.get_device_meta(&a0, &a1, &a2))
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(%err, node_key, "device_meta unreadable; history switch reads on");
                None
            })
    };
    render::node_history_ctx(
        config.as_ref(),
        drafted,
        meta.map(|m| m.backfill_auto).unwrap_or(true),
    )
}

#[derive(Deserialize)]
struct NodeHistoryForm {
    /// The checkbox: present when checked, absent when not.
    enabled: Option<String>,
}

/// The History switch (devices.node.md §3.9, amendment 2026-09-25): the automatic download
/// after a reconnect takes effect at once, and what the Node must change to keep (or stop
/// keeping) history goes into its draft, sent with the rest by "Send to device". Nothing is
/// drafted when the Node already reports the wanted `history_enable`: the change would only
/// cost it a restart and, on the debug build, the history it holds.
async fn set_node_history(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
    Form(form): Form<NodeHistoryForm>,
) -> Response {
    let node_key = node_key.to_lowercase();
    let cmd = Cmd::new(&state, &current_user, "set_sticker_config");
    let Some(hostname) = hostname_for_known_node(&state, &node_key) else {
        return cmd.fail(&tr("monitoring.shared.unknown_node"));
    };
    match apply_history(
        &state,
        &current_user,
        &hostname,
        &node_key,
        form.enabled.is_some(),
    )
    .await
    {
        Ok((enabled, drafted)) => cmd.ok(&tr(match (enabled, drafted) {
            (true, false) => "monitoring.history_switch.saved_on",
            (false, false) => "monitoring.history_switch.saved_off",
            (_, true) => "monitoring.history_switch.saved_to_draft",
        })),
        Err(message) => cmd.fail(&message),
    }
}

/// After a sensor is switched on or off: while history is on (the draft's `history_enable`,
/// else the Node's), drafts the `history_channels` of the now active sensors, or drops a stale
/// draft of it when the Node already reports that set (node-firmware-v2.0.0.md §6, amendment
/// 2026-10-28, #160). With no active sensor nothing is drafted: the Node keeps its list.
/// `true` when a draft was written.
async fn redraft_history_channels(
    state: &AppState,
    current_user: &CurrentUser,
    hostname: &str,
    node_key: &str,
) -> bool {
    let (config, node, fw_version) = {
        let device = state.store.snapshot_device(hostname);
        (
            device
                .as_ref()
                .and_then(|d| d.node_configs.get(node_key).cloned()),
            device.as_ref().and_then(|d| d.nodes.get(node_key).cloned()),
            device.and_then(|d| render::node_fw_version(&d, node_key)),
        )
    };
    let reported_on = config.as_ref().and_then(|e| {
        e.config
            .get(render::HISTORY_ENABLE_KEY)
            .and_then(serde_json::Value::as_bool)
    });
    let drafted_on = state
        .pending
        .list(hostname, node_key)
        .await
        .ok()
        .and_then(|rows| {
            rows.into_iter()
                .find(|r| r.param_key == render::HISTORY_ENABLE_KEY)
                .and_then(|r| r.desired_value)
                .and_then(|v| serde_json::from_str::<bool>(&v).ok())
        });
    if drafted_on.or(reported_on) != Some(true)
        || crate::pending::firmware_predates_v200(fw_version.as_ref())
    {
        return false;
    }
    let Some(channels) = render::history_channels(config.as_ref(), node.as_ref()) else {
        return false;
    };
    let reported = config.as_ref().and_then(|e| {
        e.config
            .get(render::HISTORY_CHANNELS_KEY)
            .and_then(serde_json::Value::as_str)
    });
    if reported.is_some_and(|r| render::same_history_channels(r, &channels)) {
        if let Err(err) = state
            .pending
            .remove(hostname, node_key, render::HISTORY_CHANNELS_KEY)
            .await
        {
            tracing::warn!(%err, node_key, "failed to drop a history channels draft");
        }
        return false;
    }
    let row = portal_storage::NewPendingConfig {
        hostname: hostname.to_string(),
        node_key: node_key.to_string(),
        param_key: render::HISTORY_CHANNELS_KEY.to_string(),
        kind: portal_storage::PENDING_KIND_CONFIG.to_string(),
        desired_value: Some(json!(channels).to_string()),
        author: current_user.username.clone(),
    };
    match state.pending.upsert(row).await {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(%err, node_key, "failed to write the history channels draft");
            false
        }
    }
}

/// The History switch's write, shared by its own POST and the Device configuration card:
/// `device_meta.backfill_auto` at once, the Node's half into the draft. `Ok((enabled,
/// drafted))`; `Err` is the message to show.
async fn apply_history(
    state: &AppState,
    current_user: &CurrentUser,
    hostname: &str,
    node_key: &str,
    enabled: bool,
) -> Result<(bool, bool), String> {
    let (state, current_user) = (state.clone(), current_user.clone());
    let (hostname, node_key) = (hostname.to_string(), node_key.to_string());
    // No active sensor, nothing to record (node-firmware-v2.0.0.md §6, amendment 2026-10-28):
    // refused before anything is stored.
    if enabled {
        let device = state.store.snapshot_device(&hostname);
        let config = device.as_ref().and_then(|d| d.node_configs.get(&node_key));
        let node = device.as_ref().and_then(|d| d.nodes.get(&node_key));
        if render::history_channels(config, node).is_none() {
            return Err(tr("monitoring.history_switch.no_active_sensors"));
        }
    }
    let saved = super::edit_device_meta(
        &state,
        &current_user,
        &hostname,
        THRESHOLD_SCOPE_NODE,
        &node_key,
        |row| row.backfill_auto = enabled,
    )
    .await;
    if let Err(err) = &saved {
        tracing::warn!(%err, node_key, "failed to save the history switch");
        return Err(tr("monitoring.meta.err_save_failed"));
    }

    let (config, node, fw_version) = {
        let device = state.store.snapshot_device(&hostname);
        (
            device
                .as_ref()
                .and_then(|d| d.node_configs.get(&node_key).cloned()),
            device
                .as_ref()
                .and_then(|d| d.nodes.get(&node_key).cloned()),
            device.and_then(|d| render::node_fw_version(&d, &node_key)),
        )
    };
    let node_enabled = config.as_ref().and_then(|e| {
        e.config
            .get(render::HISTORY_ENABLE_KEY)
            .and_then(serde_json::Value::as_bool)
    });

    // What the Node is asked to change. The channel list goes only with a switch-on from off
    // or unknown, and not when the Node already reports that list (a boot ConfigDump carries
    // it): resending it would cost a restart. v2.0.0 replaced the `history_sensors` mask with
    // `history_channels` (node-firmware-v2.0.0.md §6).
    let mut drafts: Vec<(&str, serde_json::Value)> = Vec::new();
    if node_enabled != Some(enabled) {
        drafts.push((render::HISTORY_ENABLE_KEY, json!(enabled)));
        let channels = render::history_channels(config.as_ref(), node.as_ref()).unwrap_or_default();
        let reported = config.as_ref().and_then(|e| {
            e.config
                .get(render::HISTORY_CHANNELS_KEY)
                .and_then(serde_json::Value::as_str)
        });
        // As a set (MR !129 review): the same columns in another order record the same.
        let same = reported.is_some_and(|r| render::same_history_channels(r, &channels));
        if enabled && !same && !channels.is_empty() {
            drafts.push((render::HISTORY_CHANNELS_KEY, json!(channels)));
        }
    }
    // v2.0.0 is the minimum Node firmware (amended 2026-10-14): nothing is drafted for an
    // older one.
    if !drafts.is_empty() && crate::pending::firmware_predates_v200(fw_version.as_ref()) {
        let refusal = crate::pending::SendRefusal::FirmwareTooOld {
            keys: drafts.iter().map(|(k, _)| k.to_string()).collect(),
            version: fw_version
                .map(|(a, b, c)| format!("{a}.{b}.{c}"))
                .unwrap_or_default(),
        };
        return Err(refusal.message(tr));
    }
    let drafted_keys: Vec<&str> = drafts.iter().map(|(k, _)| *k).collect();
    for key in [render::HISTORY_ENABLE_KEY, render::HISTORY_CHANNELS_KEY] {
        if drafted_keys.contains(&key) {
            continue;
        }
        if let Err(err) = state.pending.remove(&hostname, &node_key, key).await {
            tracing::warn!(%err, node_key, key, "failed to drop a history draft");
        }
    }
    for (key, value) in &drafts {
        let row = portal_storage::NewPendingConfig {
            hostname: hostname.clone(),
            node_key: node_key.clone(),
            param_key: key.to_string(),
            kind: portal_storage::PENDING_KIND_CONFIG.to_string(),
            desired_value: Some(value.to_string()),
            author: current_user.username.clone(),
        };
        if let Err(err) = state.pending.upsert(row).await {
            tracing::warn!(%err, node_key, key, "failed to write the history draft");
            return Err(tr("monitoring.pending.err_draft_failed"));
        }
    }

    super::audit_action(
        &state,
        &current_user,
        "node_history_set",
        &node_key,
        json!({
            "enabled": enabled,
            "drafted": drafts.iter().map(|(k, v)| json!({ "key": k, "value": v })).collect::<Vec<_>>(),
        }),
        true,
    )
    .await;

    // Repaints the draft banner and, through it, this card for every open tab.
    state.pending.nudge(&hostname, &node_key);
    if !drafts.is_empty() {
        send_after_save(&state, &current_user, &hostname, &node_key).await;
    }
    Ok((enabled, !drafts.is_empty()))
}

// ---------------------------------------------------------------------
// POST /nodes/{node_key}/config
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct NodeConfigForm {
    key: String,
    value: String,
}

/// Coerces the form's raw text `value` into the JSON type
/// `fiber_protocol::validate_sticker_config` expects for `key`, per its
/// `KeySpec`. An unknown key (not in `STICKER_SETTABLE`) or a value that
/// fails to coerce is passed through as a plain JSON string — either way,
/// `validate_sticker_config` still runs on the result and produces the
/// correct per-key 422 reason (`"unknown key"` / `"read-only key"` /
/// `"expected …"`), so this function never needs to duplicate that check.
fn coerce_config_value(key: &str, raw: &str) -> serde_json::Value {
    let raw = raw.trim();
    match fiber_protocol::STICKER_SETTABLE
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, spec)| *spec)
    {
        Some(fiber_protocol::StickerConfigKeySpec::Bool) => match raw.to_ascii_lowercase().as_str()
        {
            "true" => serde_json::Value::Bool(true),
            "false" => serde_json::Value::Bool(false),
            _ => serde_json::Value::String(raw.to_string()),
        },
        Some(
            fiber_protocol::StickerConfigKeySpec::UintRange(_, _)
            | fiber_protocol::StickerConfigKeySpec::UintRangeOrZero(_, _),
        ) => match raw.parse::<u64>() {
            Ok(n) => serde_json::Value::from(n),
            Err(_) => serde_json::Value::String(raw.to_string()),
        },
        // HexStr/Enum are already strings on the wire; unknown keys also
        // fall through as strings — see doc comment.
        _ => serde_json::Value::String(raw.to_string()),
    }
}

/// **A draft write since B4** (devices.node.md §3.5) — the config twin of
/// [`set_node_threshold`].
///
/// Validation is unchanged: `coerce_config_value` types the raw form text
/// against `STICKER_SETTABLE`, `validate_sticker_config` rejects an unknown
/// key, a read-only key or a value outside its range, and only then does
/// the value reach the draft. What changed is that nothing is published
/// here.
///
/// The key stored is the canonical dotted one
/// (`application.interval_report`) — the same string the wire and
/// `STICKER_SETTABLE` use — so the send walk can hand it to
/// `set_sticker_config` verbatim.
async fn set_node_config(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
    Form(form): Form<NodeConfigForm>,
) -> Response {
    let node_key = node_key.to_lowercase();
    const COMMAND: &str = "set_sticker_config";
    let cmd = Cmd::new(&state, &current_user, COMMAND);

    // First, before any validation that could reach the relay — see
    // `set_node_threshold`'s comment on the role sweep.
    if state.store.snapshot().devices.is_empty() {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    }
    // A draft is only meaningful for a Node the Hub actually knows — see
    // `hostname_for_known_node` for what an orphan row does to the next
    // device registered under this key. It is also what says *which* Hub
    // this command goes to (devices.hub.md §12 E).
    let Some(hostname) = hostname_for_known_node(&state, &node_key) else {
        return cmd.fail(&tr("monitoring.shared.unknown_node"));
    };

    match draft_config(
        &state,
        &current_user,
        &hostname,
        &node_key,
        &form.key,
        &form.value,
    )
    .await
    {
        Ok(()) => {}
        Err(message) => return cmd.fail(&message),
    }
    state.pending.nudge(&hostname, &node_key);
    send_after_save(&state, &current_user, &hostname, &node_key).await;
    cmd.ok(&tr("monitoring.pending.saved_sending"))
}

// ---------------------------------------------------------------------
// POST /nodes/{node_key}/device-config
// ---------------------------------------------------------------------

/// The Device configuration card's check (ui_components.md §6): every field the card
/// shows, in one request. Absent = unchanged.
#[derive(Deserialize)]
struct NodeDeviceConfigForm {
    #[serde(default)]
    interval_sample: Option<String>,
    #[serde(default)]
    interval_report: Option<String>,
    #[serde(default)]
    offline_k: Option<String>,
    /// `on` / `off`.
    #[serde(default)]
    history: Option<String>,
}

/// Saves the card and sends at once: an interval that differs from what the Node reports
/// goes into the draft (`draft_config`), the offline override into `device_meta`, the
/// History switch through `apply_history`; if anything was drafted the whole draft is
/// sent (`start_send`, the banner's Send) and the pending banner shows the wait.
async fn set_node_device_config(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
    Form(form): Form<NodeDeviceConfigForm>,
) -> Response {
    let node_key = node_key.to_lowercase();
    let cmd = Cmd::new(&state, &current_user, "set_sticker_config");
    let Some(hostname) = hostname_for_known_node(&state, &node_key) else {
        return cmd.fail(&tr("monitoring.shared.unknown_node"));
    };

    // The offline override first: it is checked before anything is drafted.
    let offline_k = match form.offline_k.as_deref() {
        None => None,
        Some(raw) => match super::parse_optional_count(raw, "offline_k") {
            Ok(v) => Some(v),
            Err(_) => return cmd.fail(&tr("monitoring.device_config.err_offline_k")),
        },
    };

    let reported = state
        .store
        .snapshot_device(&hostname)
        .and_then(|d| d.node_configs.get(&node_key).cloned())
        .map(|e| e.config);
    let mut drafted = false;
    for (key, raw) in [
        ("application.interval_sample", &form.interval_sample),
        ("application.interval_report", &form.interval_report),
    ] {
        let Some(raw) = raw.as_deref().map(str::trim) else {
            continue;
        };
        let same = reported
            .as_ref()
            .and_then(|c| c.get(key))
            .is_some_and(|v| v.to_string().trim_matches('"') == raw);
        if same {
            // Back to what the Node has: nothing to send for this key.
            if let Err(err) = state.pending.remove(&hostname, &node_key, key).await {
                tracing::warn!(%err, node_key, key, "failed to drop an interval draft");
            }
            continue;
        }
        if let Err(message) =
            draft_config(&state, &current_user, &hostname, &node_key, key, raw).await
        {
            return cmd.fail(&message);
        }
        drafted = true;
    }

    if let Some(k) = offline_k {
        let saved = super::edit_device_meta(
            &state,
            &current_user,
            &hostname,
            THRESHOLD_SCOPE_NODE,
            &node_key,
            |row| row.offline_k_override = k,
        )
        .await;
        super::audit_action(
            &state,
            &current_user,
            "node_meta_set",
            &node_key,
            json!({ "offline_k_override": k }),
            saved.is_ok(),
        )
        .await;
        if saved.is_err() {
            return cmd.fail(&tr("monitoring.meta.err_save_failed"));
        }
    }

    if let Some(h) = form.history.as_deref() {
        let enabled = h == "on";
        let current = history_ctx_for(&state, &hostname, &node_key).await;
        if current.checked != enabled {
            match apply_history(&state, &current_user, &hostname, &node_key, enabled).await {
                Ok((_, d)) => drafted |= d,
                Err(message) => return cmd.fail(&message),
            }
        }
    }

    state.pending.nudge(&hostname, &node_key);
    if !drafted {
        return cmd.ok(&tr("monitoring.device_config.saved"));
    }
    match start_send(&state, &current_user, &hostname, &node_key).await {
        Ok(()) => cmd.ok(&tr("monitoring.device_config.saved_sending")),
        Err(message) => cmd.fail(&message),
    }
}

// ---------------------------------------------------------------------
// GET /nodes/{node_key}/pending, POST .../pending/send, .../pending/discard
// ---------------------------------------------------------------------

/// Loads one Node's draft and everything the banner needs to render it: the
/// merged thresholds (for a bound's "current" column) and the device's own
/// config pages (for a config key's).
///
/// Shared by the detail page, the two POSTs and the SSE nudge arm — one
/// builder, so the banner cannot say different things depending on which of
/// the four produced it.
pub(crate) async fn pending_ctx_for(
    state: &AppState,
    hostname: &str,
    node_key: &str,
    can_edit: bool,
) -> render::PendingCtx {
    let rows = state
        .pending
        .list(hostname, node_key)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, node_key, "pending draft unreadable; banner renders empty");
            Vec::new()
        });

    // The store keys Hubs lowercase and `hostname` is the display spelling, so a plain
    // `get` found no Hub: the draft's "on the device" column read "unknown" for every
    // key (F16, Nodes test 2026-09-23).
    let portal_state = state.store.snapshot();
    let device = portal_state.device(hostname);
    let node = device.and_then(|d| d.nodes.get(node_key));
    let config_entries = device
        .and_then(|d| d.node_configs.get(node_key))
        .map(|entry| entry.config.clone());

    let thresholds = merged_thresholds_for(state, hostname, node, node_key).await;

    // What Send will put on the air, from the same plan the walk sends (nodes_phase1.cs.md
    // WP5): the frame count at the Node's last data rate, and whether the Node restarts.
    let units = crate::pending::plan_send(
        &rows,
        node_key,
        &thresholds,
        &node_alarm_layout(device, node_key),
        &crate::radio::alarm::SlotTypes::of_node(device.and_then(|d| d.node_configs.get(node_key))),
    );
    let downlinks = crate::pending::downlink_count(
        &units,
        node.and_then(|n| n.dr),
        state.radio.carrier_for(node_key).carrier,
    );
    // #126 item 5: what Send would refuse (firmware older than 2.0.0, the alarm table) is shown
    // on the draft itself, from the same check `start_send` runs.
    let refusal = crate::pending::refusal(
        &units,
        device
            .and_then(|d| render::node_fw_version(d, node_key))
            .as_ref(),
    )
    .map(|r| render::SendRefusalCtx::of(&r));

    // The banner shows the bounds as the page does (UX-038); the plan above compares wire values.
    let shown: std::collections::BTreeMap<String, render::ThresholdEntryCtx> = thresholds
        .iter()
        .map(|(field, t)| {
            let t = match fiber_protocol::lorawan_field(field) {
                Some(f) => t.clone().displayed(f),
                None => t.clone(),
            };
            (field.clone(), t)
        })
        .collect();
    let now = now_unix();
    let awaiting = if rows.is_empty() {
        None
    } else {
        state
            .pending
            .node_deadline(hostname, node_key)
            .await
            .map(|d| render::AwaitingCtx::of(&d, now))
    };
    render::PendingCtx {
        downlinks,
        refusal,
        awaiting,
        ..render::pending_ctx(
            node_key,
            &rows,
            config_entries.as_ref(),
            &shown,
            can_edit,
            now_unix(),
        )
    }
}

/// The bounds the portal holds for one Node now: the Hub's echo over the stored rows
/// (`render::merge_node_thresholds`). What a draft row's "on the device" reads, what an edit
/// is compared against (N17) and what the send plan fills untouched bounds from.
async fn merged_thresholds_for(
    state: &AppState,
    hostname: &str,
    node: Option<&crate::state::NodeState>,
    node_key: &str,
) -> std::collections::BTreeMap<String, render::ThresholdEntryCtx> {
    let db_thresholds = ({
        let a0 = hostname.to_string();
        let a1 = THRESHOLD_SCOPE_NODE.to_string();
        let a2 = node_key.to_string();
        state
            .db
            .run(move |s| s.list_field_thresholds(&a0, &a1, &a2))
    })
    .await
    .unwrap_or_default();
    render::merge_node_thresholds(
        node.and_then(|n| n.field_thresholds.as_deref()),
        &db_thresholds,
    )
}

/// The Node's alarm slots, in the order of its active quantities (nodes_phase1.cs.md WP2).
fn node_alarm_layout(device: Option<&crate::state::DeviceState>, node_key: &str) -> Vec<String> {
    device
        .and_then(|d| d.nodes.get(node_key).map(|n| (d, n)))
        .map(|(d, n)| {
            let active = render::active_node_fields(d.node_configs.get(node_key), n);
            crate::radio::alarm::slot_layout(active.iter().map(|f| f.name.as_str()))
        })
        .unwrap_or_default()
}

/// The banner as the **shell** — id, `sse-swap` and all. What a page
/// includes, and what the standalone GET fragment answers.
fn render_pending(
    state: &AppState,
    current_user: &CurrentUser,
    ctx: render::PendingCtx,
) -> Response {
    super::render_fragment(
        state,
        "partials/pending_config.html",
        render::page_ctx(context! { pending => ctx }, Some(current_user)),
    )
}

/// The banner's **body**, for a swap that lands *inside* the shell.
///
/// Discard targets the shell with `hx-swap="innerHTML"` (§21 review, item
/// 11), so answering with the shell would nest a second
/// `id="pending-config-…"` inside the first: two elements with one id, the
/// inner one carrying an `sse-swap` nothing ever binds. The SSE nudge arm
/// already sends exactly this partial, which is the point — one frame
/// shape, whoever produces it.
fn render_pending_body(
    state: &AppState,
    current_user: &CurrentUser,
    ctx: render::PendingCtx,
) -> Response {
    super::render_fragment(
        state,
        "partials/pending_config_body.html",
        render::page_ctx(context! { pending => ctx }, Some(current_user)),
    )
}

/// The banner as a fragment. `CurrentUser`, not `RequireManager`: the draft
/// is shared state, and a viewer who cannot see a colleague's pending
/// change cannot understand why the Node is about to behave differently.
/// The template gates the buttons on `pending.can_edit`.
///
/// A GET fragment, so no `MUTATING_ROUTES` row (that table is the `post`
/// registry).
async fn pending_fragment(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
) -> Response {
    let node_key = node_key.to_lowercase();
    let Some(hostname) = hostname_for_known_node(&state, &node_key) else {
        return not_found();
    };
    let can_edit = render::can_edit_thresholds(&current_user);
    let ctx = pending_ctx_for(&state, &hostname, &node_key, can_edit).await;
    render_pending(&state, &current_user, ctx)
}

/// Throws the whole draft away. Manager, like the edits that made it — and
/// any manager, not only its author (decision 2026-09-07).
///
/// **Nothing is put back** (config_sync.md §13 Q20e, superseding PF-3 rule 9): the limits
/// were stored at save (Q20d) and stay. The queued frames come back out and the draft goes; a
/// limit the Node now holds differently shows the "not synchronised" badge.
///
/// **Not while the walk writes the Hub** (PF-3 review, item 1): the Hub cannot
/// be told to forget a write in flight, so a Discard waits until the walk has
/// moved on to the Node — whose wait it may still cut short (N30).
async fn discard_pending(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
) -> Response {
    let node_key = node_key.to_lowercase();
    let cmd = Cmd::new(&state, &current_user, "node_pending_discard").on(&node_key);
    let Some(hostname) = hostname_for_known_node(&state, &node_key) else {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    };

    // Held for the whole Discard (PF-3 review round 2, item E): no walk can start while the
    // revert writes the Hub, and none may be writing it now. Moved into the Discard's task below.
    let Some(discarding) = state.pending.begin_discard(&hostname, &node_key) else {
        return discard_refused(
            &state,
            &current_user,
            &node_key,
            &tr("monitoring.pending.send_in_progress"),
        );
    };

    // config_sync.md §13 Q20e: Discard reverts nothing. The limits were stored at save (Q20d)
    // and stay; only the queued frames come back out and the draft goes. A limit the Node then
    // holds differently shows the "not synchronised" badge and the resend banner.
    let reverts: Vec<crate::pending::SendUnit> = Vec::new();
    let key: Option<crate::keys::UserSigningKey> = None;

    // Everything from the revert on runs on its own task (PF-3 review round 3), which a client
    // going away cannot cut between two fields; it writes the audit row itself. The Hub writes go
    // on the relay; queued frames come back out through the radio seam.
    let audit: crate::pending::DiscardAudit = {
        let (state, user, node_key) = (state.clone(), current_user.clone(), node_key.clone());
        Box::new(
            move |result: crate::pending::DiscardResult| -> futures_util::future::BoxFuture<
                'static,
                (),
            > {
                Box::pin(async move {
                    let (details, success) = match &result {
                        Ok(done) => {
                            if let Err(err) = &done.dropped {
                                tracing::warn!(%err, node_key, "failed to discard the draft");
                            }
                            (
                                json!({
                                    "rows": done.dropped.as_ref().copied().unwrap_or(0),
                                    "frames_withdrawn": done.withdrawn,
                                    "reverted": done.reverted,
                                }),
                                done.dropped.is_ok(),
                            )
                        }
                        Err(failure) => (
                            json!({
                                "status": "error",
                                "revert_failed": failure.reason,
                                "reverted": failure.reverted,
                            }),
                            false,
                        ),
                    };
                    super::audit_action(
                        &state,
                        &user,
                        "node_pending_discard",
                        &node_key,
                        details,
                        success,
                    )
                    .await;
                })
            },
        )
    };
    let task = crate::pending::spawn_discard(
        state.pending.clone(),
        std::sync::Arc::new(crate::pending::RadioSender {
            radio: state.radio.clone(),
            relay: state.relay.clone(),
        }),
        key,
        current_user.username.clone(),
        hostname.clone(),
        node_key.clone(),
        reverts,
        discarding,
        audit,
    );
    match task.await {
        Ok(Ok(_)) => {}
        Ok(Err(failure)) => {
            let message =
                tr("monitoring.pending.discard_revert_failed").replace("{reason}", &failure.reason);
            return discard_refused(&state, &current_user, &node_key, &message);
        }
        Err(err) => {
            tracing::error!(%err, node_key, "the discard task failed");
            return cmd.fail(&tr("monitoring.pending.err_draft_failed"));
        }
    }

    let can_edit = render::can_edit_thresholds(&current_user);
    let ctx = pending_ctx_for(&state, &hostname, &node_key, can_edit).await;
    render_pending_body(&state, &current_user, ctx)
}

/// The fields whose critical band the Node holds differently from the portal, with nothing
/// pending (config_sync.md §13 Q20e), and the portal's wire-unit bands for them.
pub(crate) async fn unsynced_limits(
    state: &AppState,
    hostname: &str,
    node_key: &str,
) -> Vec<(String, render::ThresholdEntryCtx)> {
    match state.pending.list(hostname, node_key).await {
        Ok(rows) if rows.is_empty() => {}
        _ => return Vec::new(),
    }
    let device = state.store.snapshot_device(hostname);
    let node = device.as_ref().and_then(|d| d.nodes.get(node_key).cloned());
    let layout = node_alarm_layout(device.as_ref(), node_key);
    let wire = merged_thresholds_for(state, hostname, node.as_ref(), node_key).await;
    let delivery = {
        let (h, n) = (hostname.to_string(), node_key.to_string());
        state
            .db
            .run(move |s| s.list_node_delivery(&h, &n))
            .await
            .unwrap_or_default()
    };
    wire.into_iter()
        .filter(|(field, t)| {
            if !layout.contains(field) {
                return false;
            }
            let shown = match fiber_protocol::lorawan_field(field) {
                Some(f) => t.clone().displayed(f),
                None => t.clone(),
            };
            render::node_unsynced(field, &shown, delivery.iter().find(|d| &d.field == field))
                .is_some()
        })
        .collect()
}

/// "Odeslat do Nodu" (config_sync.md §13 Q20e): drafts every limit the Node holds differently
/// — its critical bounds and switch, at what the portal holds — and sends them as one message
/// by the same path as a save. The usual "awaiting settings" banner follows; the Node's ack
/// settles it and the badge goes.
async fn resync_limits(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
) -> Response {
    let node_key = node_key.to_lowercase();
    const AUDIT: &str = "node_limits_resync";
    let cmd = Cmd::new(&state, &current_user, AUDIT);
    let Some(hostname) = hostname_for_known_node(&state, &node_key) else {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    };
    let fields = unsynced_limits(&state, &hostname, &node_key).await;
    if fields.is_empty() {
        return cmd.fail(&tr("monitoring.threshold_table.node_resync_nothing"));
    }
    for (field, t) in &fields {
        let values = [
            ("critical_low", serde_json::to_string(&t.critical_low)),
            ("critical_high", serde_json::to_string(&t.critical_high)),
        ];
        let mut rows: Vec<(String, String)> = values
            .into_iter()
            .map(|(bound, v)| {
                (
                    crate::pending::threshold_param_key(field, bound),
                    v.unwrap_or_else(|_| "null".into()),
                )
            })
            .collect();
        rows.push((
            crate::pending::threshold_enabled_param_key(field),
            t.enabled.to_string(),
        ));
        for (param_key, desired) in rows {
            let row = portal_storage::NewPendingConfig {
                hostname: hostname.clone(),
                node_key: node_key.clone(),
                param_key,
                kind: portal_storage::PENDING_KIND_THRESHOLD.to_string(),
                desired_value: Some(desired),
                author: current_user.username.clone(),
            };
            if let Err(err) = state.pending.upsert(row).await {
                tracing::warn!(%err, node_key, "resync: a limit was not drafted");
                return cmd.fail(&tr("monitoring.pending.err_draft_failed"));
            }
        }
    }
    match start_send(&state, &current_user, &hostname, &node_key).await {
        Ok(()) => cmd.ok(&tr("monitoring.pending.send_started")),
        Err(message) => cmd.fail(&message),
    }
}

/// A Discard the Hub did not let through (PF-3): the banner stays as it is (`HX-Reswap: none`),
/// the draft with it, and the reason goes to `#command-status` out of band — the pattern
/// `backfill_refused` set, since the button's target is the banner.
fn discard_refused(
    state: &AppState,
    current_user: &CurrentUser,
    node_key: &str,
    message: &str,
) -> Response {
    let cmd = Cmd::new(state, current_user, "node_pending_discard").on(node_key);
    let ctx = render::page_ctx(
        context! { cmd => cmd.ctx(cmd.error(message)), oob => true },
        Some(current_user),
    );
    let mut response = super::render_fragment(state, "partials/command_status.html", ctx);
    response
        .headers_mut()
        .insert("hx-reswap", axum::http::HeaderValue::from_static("none"));
    response
}

/// Walks the draft (devices.node.md §3.5's "send configuration").
///
/// Any manager may press this, whoever drafted the values — the draft is
/// one shared intention, not a per-user queue (decision 2026-09-07).
///
/// **The walk runs on a detached task** (§21 review, defect 2). It used to
/// run inline so the operator got the outcome in the response, but a config
/// unit waits up to 180 s for the device and the request holding it can go
/// away — a closed tab, a navigation, a proxy read timeout. Dropping the
/// future mid-walk left rows in `sending`, which
/// `pending_config_body.html` renders with *both* Send and Discard disabled:
/// a draft nobody could send and nobody could throw away, until a restart.
///
/// So the route claims the Node, spawns the walk, and answers immediately
/// with "sending". The banner was always where the detail lived — it
/// repaints over SSE from the walk's own nudges, per unit — and the walk's
/// audit row is written by the task with the real outcome, exactly as
/// before.
///
/// **One walk per Node** (§21 review, item 4). The claim is taken
/// synchronously, here, before anything is spawned: two managers pressing
/// Send in the same millisecond otherwise ran two walks over one draft,
/// both waiting on confirmations that carry no request id, either of which
/// would answer to either walk.
///
/// One audit row, with the real outcome — `node_pending_send`, carrying how
/// many units were sent and, on a failure, which key stopped it and why.
async fn send_pending(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
) -> Response {
    let node_key = node_key.to_lowercase();
    const AUDIT: &str = "node_pending_send";
    let cmd = Cmd::new(&state, &current_user, AUDIT);

    let Some(hostname) = hostname_for_known_node(&state, &node_key) else {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    };
    match start_send(&state, &current_user, &hostname, &node_key).await {
        Ok(()) => cmd.ok(&tr("monitoring.pending.send_started")),
        Err(message) => cmd.fail(&message),
    }
}

/// Sends the Node its whole pending message right after a save (config_sync.md §13 Q20b): one
/// message per Node, assembled from everything still pending. While a walk is running for the
/// Node, the newest version goes once it ends; the Hub withdraws an older version still waiting
/// in its queue (`set-config --supersede`). Nothing pending, nothing sent.
pub(crate) async fn send_after_save(
    state: &AppState,
    current_user: &CurrentUser,
    hostname: &str,
    node_key: &str,
) {
    if !state.pending.auto_send() {
        return;
    }
    if state.pending.is_walking(hostname, node_key) {
        state
            .pending
            .want_resend(hostname, node_key, current_user.clone());
        return;
    }
    if let Err(message) = start_send(state, current_user, hostname, node_key).await {
        if state.pending.is_walking(hostname, node_key) {
            state
                .pending
                .want_resend(hostname, node_key, current_user.clone());
        } else {
            tracing::info!(node_key, %message, "the Node's message was not sent after the save");
        }
    }
}

/// Starts the send walk of one Node's whole draft in the background and
/// returns once it is claimed and spawned; the banner follows the walk over
/// its nudges. `Err` is the refusal to show, and nothing was sent. Shared by
/// the Node's own Send and by a save from /limits ([`send_after_save`]).
///
/// Boxed with an explicit `Send` type: the walk's task can start the next walk
/// ([`send_after_save`]), and an opaque future cannot name itself.
pub(crate) fn start_send<'a>(
    state: &'a AppState,
    current_user: &'a CurrentUser,
    hostname: &'a str,
    node_key: &'a str,
) -> futures_util::future::BoxFuture<'a, Result<(), String>> {
    Box::pin(start_send_inner(state, current_user, hostname, node_key))
}

async fn start_send_inner(
    state: &AppState,
    current_user: &CurrentUser,
    hostname: &str,
    node_key: &str,
) -> Result<(), String> {
    let hostname = hostname.to_string();
    let node_key = node_key.to_string();

    let rows = match state.pending.list(&hostname, &node_key).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(%err, node_key, "draft unreadable; refusing to send");
            return Err(tr("monitoring.pending.err_draft_failed"));
        }
    };
    if rows.is_empty() {
        return Err(tr("monitoring.pending.no_draft"));
    }

    // The bounds a drafted field is not changing, so the walk can fill
    // them: `set_lorawan_field_threshold` takes all four at once.
    let portal_state = state.store.snapshot();
    let device = portal_state.device(&hostname);
    let node = device.and_then(|d| d.nodes.get(&node_key));
    let alarm_layout = node_alarm_layout(device, &node_key);
    let slot_types =
        crate::radio::alarm::SlotTypes::of_node(device.and_then(|d| d.node_configs.get(&node_key)));
    // How long the walk waits for the Node's ack: the one answer budget, from its report
    // interval (N13, P2).
    let budget = crate::pending::downlink_answer_budget(crate::pending::interval_report_of(
        device
            .and_then(|d| d.node_configs.get(&node_key))
            .map(|entry| &entry.config),
    ));
    let db_thresholds = ({
        let a0 = hostname.clone();
        let a1 = THRESHOLD_SCOPE_NODE.to_string();
        let a2 = node_key.clone();
        state
            .db
            .run(move |s| s.list_field_thresholds(&a0, &a1, &a2))
    })
    .await
    .unwrap_or_default();
    let thresholds = render::merge_node_thresholds(
        node.and_then(|n| n.field_thresholds.as_deref()),
        &db_thresholds,
    );
    let fw_version = device.and_then(|d| render::node_fw_version(d, &node_key));
    drop(portal_state);

    let units =
        crate::pending::plan_send(&rows, &node_key, &thresholds, &alarm_layout, &slot_types);

    // MR !129 review: an alarm table that would drop a band from the Node, or v2.0.0 keys for
    // older firmware, is refused before anything is sent, with the reason.
    if let Some(refusal) = crate::pending::refusal(&units, fw_version.as_ref()) {
        tracing::info!(node_key, ?refusal, "send refused");
        return Err(refusal.message(tr));
    }

    // Claimed before the key is minted and before anything is spawned, so a
    // second Send arriving in the same millisecond is refused rather than
    // racing the first one's first write.
    let Some(claim) = state.pending.claim_send(&hostname, &node_key) else {
        return Err(tr("monitoring.pending.send_in_progress"));
    };

    let permissions = perms::permissions_for(current_user);
    let key = match state
        .keys
        .ensure_user_key(current_user, &hostname, &permissions)
        .await
    {
        Ok(key) => key,
        Err(err) => {
            tracing::error!(%err, hostname, "failed to mint signing key");
            return Err(super::command::signing_key_error(&err));
        }
    };

    // Subscribed BEFORE the first relay call — and before the spawn, so the
    // subscription is older than the task: a Node that answers unusually
    // fast must not beat the subscription that is waiting for it.
    //
    // The command-result stream and not the fleet firehose: the walk holds
    // this receiver for up to three minutes, and on the main broadcast it
    // was competing with every reading from every device for 256 slots —
    // a lap there drops the one answer the walk exists to hear.
    let confirmations = state.store.subscribe_command_results();

    // The audit row is written by the task, minutes from now, where the
    // per-request language task-local no longer exists (a spawned future
    // does not inherit task-locals — see `i18n.rs`). Resolved here so the
    // operator's own language still reaches their audit row and the
    // messages it carries.
    let lang = crate::i18n::current_lang();
    let drafted_keys = rows.len();

    let task_state = state.clone();
    let task_user = current_user.clone();
    let task_hostname = hostname.clone();
    let task_node_key = node_key.clone();
    // S38-1: the walk's audit rows name the client that pressed Send.
    let client_ip = crate::auth::current_client_ip();
    tokio::spawn(crate::auth::scope_client_ip(client_ip, async move {
        run_send_task(
            task_state,
            task_user,
            task_hostname,
            task_node_key,
            claim,
            key,
            units,
            confirmations,
            budget,
            lang,
            drafted_keys,
        )
        .await;
    }));

    // The banner is the detail; this is only "it started". It repaints from
    // the walk's first nudge, which lands within milliseconds.
    state.pending.nudge(&hostname, &node_key);
    Ok(())
}

/// The body of [`start_send`]'s task. A save that arrived while it ran
/// ([`send_after_save`]) starts the next walk once this one let go of the Node.
#[allow(clippy::too_many_arguments)]
async fn run_send_task(
    task_state: AppState,
    task_user: CurrentUser,
    task_hostname: String,
    task_node_key: String,
    claim: crate::pending::SendClaim,
    key: crate::keys::UserSigningKey,
    units: Vec<crate::pending::SendUnit>,
    confirmations: tokio::sync::broadcast::Receiver<crate::state::ChangeEvent>,
    budget: std::time::Duration,
    lang: &'static str,
    drafted_keys: usize,
) {
    const AUDIT: &str = "node_pending_send";
    {
        // Moved in, so the Node stays claimed for exactly as long as the
        // walk runs and is released however it ends — including a panic or
        // an abort.
        let _claim = claim;
        let state = task_state;
        let current_user = task_user;
        let hostname = task_hostname;
        let node_key = task_node_key;
        let t = |key: &str| crate::i18n::lookup(lang, key);

        // The device leg goes through the in-process control.radio seam, confirmed by the
        // Node's own ack for the seq it was sent under; the Hub-side threshold write stays on
        // the relay. See `pending::RadioUnitSender`.
        let sender = crate::pending::RadioUnitSender {
            radio: &state.radio,
            relay: &state.relay,
        };
        let result = crate::pending::run_send_walk(
            &state.pending,
            &sender,
            &key,
            &current_user.username,
            &hostname,
            &node_key,
            &units,
            confirmations,
            budget,
        )
        .await;

        // PF-3: the walk writes a field's bounds through the moment the Hub takes them and the
        // alarm layout once the Node confirms its table (`pending::WriteThrough`), so a Node that
        // answers late or not at all no longer leaves the portal on the old band. This nudge
        // lets the detail's thresholds table re-read what landed (N16).
        state.pending.nudge(&hostname, &node_key);

        match &result {
            Ok(sent) => {
                super::audit_action(
                    &state,
                    &current_user,
                    AUDIT,
                    &node_key,
                    json!({ "units": sent, "keys": drafted_keys, "status": "success" }),
                    true,
                )
                .await;
            }
            // PF-3 (decision Hynek 2026-09-27): a bound is set once the Hub and the portal have
            // it — telemetry evaluation is the primary alarm path, the Node's own table an
            // extension. A walk that stopped at the Node's part with no config key in it
            // changed every bound it carried, so it is audited as the success it is.
            Err(crate::pending::SendStop::NodeUndelivered {
                param_key,
                message,
                config_keys,
            }) if config_keys.is_empty() => {
                super::audit_action(
                    &state,
                    &current_user,
                    AUDIT,
                    &node_key,
                    json!({
                        "keys": drafted_keys,
                        "status": "success",
                        "node_delivery": "undelivered",
                        "stopped_at": param_key,
                        "message": t("monitoring.pending.bound_applied_node_undelivered")
                            .replace("{key}", param_key),
                        "reason": message,
                    }),
                    true,
                )
                .await;
            }
            Err(stop) => {
                let message = match stop {
                    // A refusal from the Hub and a draft the portal could
                    // not write read the same way to the operator: the
                    // thing that stopped, and why.
                    crate::pending::SendStop::Relay {
                        param_key, message, ..
                    }
                    | crate::pending::SendStop::Storage { param_key, message } => {
                        format!("{param_key}: {message}")
                    }
                    // Config keys only the Node holds (intervals) did not land. Any bound in
                    // the same unit is in effect all the same (see the arm above).
                    crate::pending::SendStop::NodeUndelivered {
                        param_key, message, ..
                    } => format!(
                        "{} ({message})",
                        t("monitoring.pending.device_did_not_confirm")
                            .replace("{key}", param_key)
                            // The budget this walk waited, not the floor: a 900 s Node
                            // waits 1860 s, and "180 s" sent the operator looking for a
                            // timeout that never happened.
                            .replace("{seconds}", &budget.as_secs().to_string())
                    ),
                    crate::pending::SendStop::Superseded { param_key, .. } => {
                        t("monitoring.pending.superseded").replace("{key}", param_key)
                    }
                    crate::pending::SendStop::Unreadable { param_key } => {
                        t("monitoring.pending.unreadable").replace("{key}", param_key)
                    }
                    crate::pending::SendStop::Refused { refusal, .. } => refusal.message(t),
                    crate::pending::SendStop::Nothing => t("monitoring.pending.no_draft"),
                };
                super::audit_action(
                    &state,
                    &current_user,
                    AUDIT,
                    &node_key,
                    json!({
                        "keys": drafted_keys,
                        "stopped_at": stop.param_key(),
                        "status": "error",
                        "message": message,
                        // PF-3 review, item 10: the fields whose Hub write landed before the
                        // stop. Those bounds are in effect; their Node part is `undelivered`.
                        "bounds_in_effect": match stop {
                            crate::pending::SendStop::Relay { applied, .. }
                            | crate::pending::SendStop::Superseded { applied, .. } => applied.clone(),
                            _ => Vec::new(),
                        },
                    }),
                    false,
                )
                .await;
            }
        }

        // #166: a frame the network server discarded as too big goes again once, split
        // into what it said it carries.
        if state
            .pending
            .retry_after_size_discard(&hostname, &node_key, result.as_ref().err())
        {
            tracing::info!(
                node_key,
                "the Node's message was too big for its data rate; sending it again split"
            );
            state
                .pending
                .want_resend(&hostname, &node_key, current_user.clone());
        }

        state.pending.nudge(&hostname, &node_key);
        drop(_claim);
        if let Some(user) = state.pending.take_resend(&hostname, &node_key) {
            // config_sync.md §13 Q20b: the newest version, now that the Node is free.
            send_after_save(&state, &user, &hostname, &node_key).await;
        }
    }
}

// ---------------------------------------------------------------------
// POST /nodes/{node_key}/refresh-config
// ---------------------------------------------------------------------

async fn refresh_config(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
) -> Response {
    let node_key = node_key.to_lowercase();
    const COMMAND: &str = "get_sticker_settings";
    let cmd = Cmd::new(&state, &current_user, COMMAND);

    // The node must be one we already know; the seam then addresses it by
    // DevEUI, which is the `node_key` (node read-path unification).
    if hostname_for_known_node(&state, &node_key).is_none() {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    }

    // PI2: the config refresh runs through the in-process `control.radio` seam,
    // not a signed relay to the v1 `fiber_app` agent. The role gate above plus
    // the audit row is the authorization (plan/portal/p2p_integration.md §2.1).
    //
    // It asks for the small boot settings-info (`node-send get-settings`,
    // sticker-firmware #429), never a full `get-config`: that answer runs to
    // 6–14 pages (decision 2026-09-23, node_reported_state.md §2.3). The answer
    // merges on the Hub and lands on the node's retained `config` topic, which
    // repaints the config card over SSE.
    cmd.radio(
        state.radio.get_settings(&node_key).await,
        &tr("monitoring.shared.config_refresh_queued"),
        "node_config_refresh",
        &node_key,
        json!({ "transport": "control.radio" }),
    )
    .await
}

/// "Read from the device" on the Diagnostics Device card (nodes_phase1.cs.md WP7): GetInfo
/// and GetSettings, two small downlinks, so device info and config info refresh together.
/// Manager, like the config refresh beside it; the answers repaint both cards over SSE.
async fn read_device(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
) -> Response {
    let node_key = node_key.to_lowercase();
    let cmd = Cmd::new(&state, &current_user, "get_sticker_info");
    if hostname_for_known_node(&state, &node_key).is_none() {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    }
    let result = match state.radio.get_info(&node_key).await {
        Ok(_) => state.radio.get_settings(&node_key).await,
        Err(e) => Err(e),
    };
    cmd.radio(
        result,
        &tr("monitoring.shared.device_read_queued"),
        "node_read_device",
        &node_key,
        json!({ "transport": "control.radio" }),
    )
    .await
}

/// "Zjistit stav" on the Device card of a P2P Node (p2p_control_commands.md §5): one `0x91`
/// DevStatusReq. The answer repaints the card's device status section over SSE.
async fn dev_status(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
) -> Response {
    let node_key = node_key.to_lowercase();
    let cmd = Cmd::new(&state, &current_user, "node_dev_status");
    if hostname_for_known_node(&state, &node_key).is_none() {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    }
    cmd.radio(
        state.radio.dev_status(&node_key).await,
        &tr("monitoring.shared.dev_status_queued"),
        "node_dev_status",
        &node_key,
        json!({ "transport": "control.radio" }),
    )
    .await
}

// ---------------------------------------------------------------------
// POST /nodes, POST /nodes/{node_key}/remove, POST /nodes/{node_key}/keys/reveal
// ---------------------------------------------------------------------

/// The required fields default to blank too (O-3): a POST without one gets the dialog's own
/// flash from the checks in [`add_node`], not a bare 422 from the extractor.
#[derive(Deserialize)]
struct AddNodeForm {
    #[serde(default)]
    serial: String,
    #[serde(default)]
    dev_eui: String,
    /// Not sent by the dialog any more: registration always goes to the Server
    /// (devices.node.md §5, dodatek 2026-10-13). Read only to refuse a POST
    /// that names another Hub.
    #[serde(default)]
    hub: Option<String>,
    /// `lorawan` | `p2p`, the dialog's radio choice. Blank — a caller posting by hand — takes
    /// the registration default, the Hub's `radio.mode` (see the handler).
    #[serde(default)]
    radio: String,
    /// Optional; blank means the fleet's `default` profile — see the handler.
    #[serde(default)]
    profile: String,
    #[serde(default)]
    app_key: String,
    /// Optional (#86). The dialog prefills the Hub's `radio.join_eui`; blank
    /// sends nothing and the Hub applies that same default — see the handler.
    #[serde(default)]
    join_eui: String,
    /// Optional, and blank falls back to the serial — see the handler.
    #[serde(default)]
    name: String,
    /// Operator's note. Portal-side only, like every other `device_meta`
    /// column; it never reaches the Hub.
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
struct RemoveNodeForm {
    /// The typed confirmation, checked on the server against the node_key.
    #[serde(default)]
    confirm: String,
}

/// "Změnit rádio" (devices.node.md, dodatek 2026-09-27). Both default to blank, so a POST
/// without one gets the dialog's own refusal rather than a bare 422.
#[derive(Deserialize)]
struct MoveNodeForm {
    /// `lorawan` | `p2p`: the carrier the Node moves to.
    #[serde(default)]
    to: String,
    /// The typed confirmation, checked on the server against the node_key.
    #[serde(default)]
    confirm: String,
}

/// A carrier as the forms spell it, the `--radio` value.
fn carrier_named(raw: &str) -> Option<portal_domain::Transport> {
    portal_domain::Transport::ALL
        .into_iter()
        .find(|c| c.as_str() == raw.trim())
}

/// "Cannot add X as a P2P node: its radio address A … is already used by Y" (#470 H4). The
/// DevEUIs are spelled uppercase like the address, so the shared last 8 digits read as the
/// same text in all three.
fn address_collision_sentence(dev_eui: &str, addr: &str, holders: &[String]) -> String {
    let holders: Vec<String> = holders.iter().map(|h| h.to_ascii_uppercase()).collect();
    tr("monitoring.nodes.err_address_collision")
        .replace("{dev_eui}", &dev_eui.to_ascii_uppercase())
        .replace("{addr}", &addr.to_ascii_uppercase())
        .replace("{holder_dev_eui}", &holders.join(", "))
}

/// The Hub's `AddressCollision` line, "… dev_eui X has the P2P address A (…), which Y already
/// holds on p2p; …", as its three parts. `None` for any other wording, which then keeps the
/// generic sentence.
fn hub_collision_parts(line: &str) -> Option<(String, String, String)> {
    let rest = line.split_once("dev_eui ")?.1;
    let (dev_eui, rest) = rest.split_once(" has the P2P address ")?;
    let addr = rest.get(..8)?;
    let holder = rest
        .split_once(", which ")?
        .1
        .split_once(" already holds")?
        .0;
    let hex = |s: &str, n: usize| s.len() == n && s.chars().all(|c| c.is_ascii_hexdigit());
    (hex(dev_eui, 16) && hex(addr, 8) && hex(holder, 16))
        .then(|| (dev_eui.to_string(), addr.to_string(), holder.to_string()))
}

/// The flash of a registration the Hub refused. Its own words, verbatim: on a v2 Hub this is
/// the "legacy LoRaWAN stack disabled" refusal, and paraphrasing it into "registration failed"
/// would throw away the only sentence that says what to do instead. An `AddressCollision`
/// (#470 H4) becomes the same plain sentence the pre-check shows, built from the DevEUIs the
/// Hub's line names; a line that does not parse leads with the generic sentence and keeps the
/// Hub's words after it.
fn add_failure_reason(outcome: &crate::relay::CommandOutcome) -> String {
    let hub = outcome.message.clone().filter(|m| !m.trim().is_empty());
    match (outcome.error_code, hub) {
        (Some(code @ crate::relay::error_code::ADDRESS_COLLISION), Some(hub)) => {
            match hub_collision_parts(&hub) {
                Some((dev_eui, addr, holder)) => {
                    address_collision_sentence(&dev_eui, &addr, &[holder])
                }
                None => format!(
                    "{} ({hub})",
                    tr(&format!("monitoring.command_error.{code}"))
                ),
            }
        }
        (_, Some(hub)) => hub,
        (_, None) => tr("monitoring.nodes.err_add_failed"),
    }
}

fn node_list_flash(code: &str) -> String {
    // "Already registered on <carrier>" (decision #23) and "<carrier> is switched off on this
    // Hub": the carrier is the code's suffix.
    for (prefix, key) in [
        (
            "registered_on_",
            "monitoring.nodes.err_registered_on_carrier",
        ),
        ("radio_off_", "monitoring.nodes.err_radio_off"),
    ] {
        if let Some(carrier) = code.strip_prefix(prefix).and_then(carrier_named) {
            return tr(key).replace(
                "{carrier}",
                &tr(&format!("monitoring.transport.{}", carrier.as_str())),
            );
        }
    }
    // A command refused before anything was signed: `not_paired` and its two siblings (#109).
    if let Some(message) = super::command::refusal_message(code) {
        return message;
    }
    let key = match code {
        "invalid_radio" => "monitoring.nodes.err_invalid_radio",
        "already_exists" => "monitoring.nodes.err_already_exists",
        "invalid_dev_eui" => "monitoring.nodes.err_invalid_dev_eui",
        "invalid_app_key" => "monitoring.nodes.err_invalid_app_key",
        "invalid_join_eui" => "monitoring.nodes.err_invalid_join_eui",
        "invalid_serial" => "monitoring.nodes.err_invalid_serial",
        "no_device" => "monitoring.shared.no_device_yet",
        "no_server" => "monitoring.nodes.err_no_server",
        "add_failed" => "monitoring.nodes.err_add_failed",
        "confirm_mismatch" => "monitoring.nodes.err_remove_confirm_mismatch",
        "remove_failed" => "monitoring.nodes.err_remove_failed",
        "registered" => "monitoring.nodes.registered_ok",
        "removed" => "monitoring.nodes.removed_ok",
        _ => "monitoring.shared.something_went_wrong",
    };
    tr(key)
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Registers a Node (devices.node.md §5, §7's WP4.4).
///
/// **Everything is checked before anything is published.** A registration
/// that reaches the Hub with a malformed DevEUI comes back as a signed
/// command failure minutes later, and the operator has by then closed the
/// dialog; the shape checks the agent makes (`manager.rs`'s `node-add` arm)
/// run here first, so a typo is a red field rather than a mystery.
///
/// **The wire verb is the v2 `node-add`** (control.radio.md §7): the signed
/// twin of `proximosctl control.radio node-add`, executed by the agent
/// through `control_radio`. The legacy `add_lorawan_sticker` this handler
/// used to send is what the agent's radio gate refuses on any Hub whose
/// `radio.mode` names a transport, which is every v2 Hub. On a Hub with no
/// v2 radio config the agent refuses `node-add` instead, and its own
/// sentence is surfaced verbatim in the Nodes list's error flash rather than
/// paraphrased into "failed".
///
/// **The carrier is the dialog's radio choice**, preselected from the Hub's `radio.mode`
/// ([`crate::radio::registration_default`]) and refused when this Hub has it switched off. A
/// DevEUI the Hub's registry already holds is refused with the carrier it is on: a Node is
/// registered on one carrier only (decision #23, amended 2026-09-27).
///
/// **The profile is the rendered config's `device_profiles.otaa.<name>`**,
/// and the dialog has no field for it: one hardware kind ships, the fleet
/// defines one profile, and the portal receives no signal naming the
/// others. `default` (`control_radio::lorawan::DEFAULT_PROFILE`) goes on
/// the wire, and an unknown name is the agent's `InvalidArgument` to report.
///
/// **The name is the operator's, and the serial when they leave it blank.**
/// The agent records a `name` when given one; the serial the operator did
/// type stands in for a blank field, because it is the identity printed on
/// the device. Same rule as `POST /beacons/add`.
///
/// **The JoinEUI is optional and defaults on the Hub** (#86, control.radio.md
/// §7 amendment 2026-09-22). The dialog prefills the Hub's `radio.join_eui`
/// from `system/info.lorawan.join_eui`; the operator can overwrite it, and a
/// blank field puts nothing on the wire, so the agent applies that same
/// default — the CLI's `node-add` without `--join-eui`. It is not a
/// formality: ChirpStack v4 accepts any JoinEUI on join and overwrites the
/// stored one, so a wrong value is never refused, only silently diverged.
///
/// **The description never reaches the Hub.** There is no wire field for
/// it, so it is written to `device_meta` after the registration succeeds —
/// the order `add_beacon` uses for the Beacon's serial, and for the same
/// reason: a refused registration must leave no row behind for the next
/// device at that DevEUI to inherit (§19.5).
///
/// **The AppKey never leaves this function by any path but the wire.** Not
/// into the audit details, not into the flash, not into a log line. It goes
/// into the signed params and nowhere else; a test greps for it. That is a
/// statement about where the value is *visible*, not about how many copies
/// of it exist in memory — see the `Zeroizing` comment in the body for the
/// three that outlive the handler.
async fn add_node(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Query(hub): Query<super::HubQuery>,
    Form(form): Form<AddNodeForm>,
) -> Response {
    const COMMAND: &str = "node-add";
    let cmd = Cmd::new(&state, &current_user, COMMAND);

    let dev_eui = form.dev_eui.trim().to_lowercase();
    let serial = form.serial.trim().to_string();
    // Zeroized on drop, like `keys`' seeds and the relay's (§21 review, item
    // 19). This covers the normalised copy and only that copy.
    //
    // **It is not end to end, and the doc comment above is about scope, not
    // about memory** (§21 review round 2, item 4). Three copies of the key
    // outlive this one in freed heap, none of them reachable from here:
    //
    // 1. `form.app_key` — a plain `String` axum filled before the handler
    //    ran. Closing it needs a `Deserialize` on a zeroizing wrapper.
    // 2. the `Value::String` inside the `json!` params below —
    //    `CommandRelay::execute` takes `serde_json::Value` **by value**, so
    //    the copy is moved out of this function and dropped inside the
    //    relay; a `Zeroizing` wrapper here could only zero a *clone*, which
    //    adds a copy rather than removing one.
    // 3. the serialized command body — `serde_json::to_vec` of the params,
    //    plus whatever the MQTT client buffers of it.
    //
    // Closing 2 and 3 means a zeroizing params type threaded through
    // `relay.execute`, its signing path and the publish buffer, which is a
    // change to every caller of the relay for a secret that is already on
    // the wire. Recorded in devices.backend.md §21.12 rather than pretended
    // away.
    let app_key = zeroize::Zeroizing::new(form.app_key.trim().to_lowercase());
    if !is_hex(&dev_eui, 16) {
        return Redirect::to("/nodes?error=invalid_dev_eui").into_response();
    }
    if !is_hex(&app_key, 32) {
        return Redirect::to("/nodes?error=invalid_app_key").into_response();
    }
    // A STICKER serial is a u32, typed in decimal (`2162165722`) or hex (`80e007da`), the forms
    // the Hub's registry canonicalises (node_registry.md §2). Anything else the Hub refuses, so
    // it is refused here first.
    if control_radio::p2p::session::canonical_serial(&serial).is_none() {
        return Redirect::to("/nodes?error=invalid_serial").into_response();
    }
    // The dialog's radio choice; blank takes the default the dialog preselects, the Server's
    // `radio.mode` and P2P on a `dual` Hub. A carrier the Server has switched off is refused
    // before anything is signed: the Hub's mode gate would refuse it too, later and less
    // plainly (devices.node.md, dodatek 2026-09-27).
    let mode = state.radio.mode();
    let carrier = match form.radio.trim() {
        "" => crate::radio::registration_default(mode),
        named => match carrier_named(named) {
            Some(carrier) => carrier,
            None => return Redirect::to("/nodes?error=invalid_radio").into_response(),
        },
    };
    if !crate::radio::carrier_enabled(mode, carrier) {
        return Redirect::to(&format!("/nodes?error=radio_off_{}", carrier.as_str()))
            .into_response();
    }
    // A Node is registered on exactly one carrier (decision #23, amended 2026-09-27). A DevEUI
    // the registry of this Hub already holds is refused here, naming that carrier, before
    // anything is signed; the Hub's `node-add` refuses it too, and a carrier change is "Změnit
    // rádio". Ahead of the store's own check below, which cannot say where the Node is.
    let at = state.radio.carrier_for(&dev_eui);
    if let Some(on) = at.registered_on() {
        return Redirect::to(&format!("/nodes?error=registered_on_{}", on.as_str()))
            .into_response();
    }
    // #470 H4: on P2P the Hub keys a Node by its DevEUI's low 32 bits, so a second Node at an
    // address another P2P Node holds is refused here, naming both, before anything is signed.
    // The Hub refuses it too (`AddressCollision`), which `add_failure_reason` words the same.
    if carrier == portal_domain::Transport::P2p {
        if let Some(collision) = state.radio.p2p_address_collision(&dev_eui) {
            return nodes_page_with(
                &state,
                &current_user,
                NodeListQuery::default(),
                Some(address_collision_sentence(
                    &dev_eui,
                    &collision.addr,
                    &collision.holders,
                )),
            )
            .await;
        }
    }
    // The JoinEUI is LoRaWAN's join parameter: a P2P registration carries none, whatever the
    // form posted (the dialog shows no field on a P2P Hub).
    let join_eui = super::non_empty(form.join_eui.trim().to_lowercase())
        .filter(|_| carrier == portal_domain::Transport::Lorawan);
    if join_eui.as_deref().is_some_and(|j| !is_hex(j, 16)) {
        return Redirect::to("/nodes?error=invalid_join_eui").into_response();
    }
    let name = super::non_empty(form.name.trim().to_string()).unwrap_or_else(|| serial.clone());
    let description = super::non_empty(form.description.trim().to_string());
    let profile = super::non_empty(form.profile.trim().to_string())
        .unwrap_or_else(|| control_radio::lorawan::DEFAULT_PROFILE.to_string());

    let portal_state = state.store.snapshot();
    // A DevEUI ANY Hub already knows is a real conflict, and reporting
    // success would leave the operator believing they registered a device
    // they did not. The key is globally unique, so the check spans the
    // store (devices.hub.md §12 E).
    if portal_state.hub_of_node(&dev_eui).is_some() {
        return Redirect::to("/nodes?error=already_exists").into_response();
    }
    drop(portal_state);
    // Nodes are registered on the Server, never on a Client (devices.node.md §5, dodatek
    // 2026-10-13), and the dialog names no Hub. A `hub` posted anyway (a hand-made POST, an old
    // form, `?hub=`) is refused unless it names the Server: a Client refuses `node-add` itself,
    // and the checks above were made against the Server's radio, not against the one named.
    let Some(hostname) = registration_server(&state) else {
        let code = if state.store.snapshot().devices.is_empty() {
            "no_device"
        } else {
            "no_server"
        };
        return Redirect::to(&format!("/nodes?error={code}")).into_response();
    };
    let wanted = form
        .hub
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .or_else(|| hub.hub.as_deref().map(str::trim).filter(|h| !h.is_empty()));
    if let Some(wanted) = wanted {
        if !wanted.eq_ignore_ascii_case(&hostname) {
            return nodes_page_with(
                &state,
                &current_user,
                NodeListQuery::default(),
                Some(
                    tr("monitoring.nodes.err_hub_not_server")
                        .replace("{hub}", wanted)
                        .replace("{server}", &hostname),
                ),
            )
            .await;
        }
    }

    // control.radio.md §7's record: `{radio, profile, serial, dev_eui, app_key}`,
    // all required, plus the optional `name` and `join_eui`. OTAA is implied —
    // it is the only activation phase 1 enrols, and the agent's arm knows no
    // other.
    let mut params = json!({
        "radio": carrier.as_str(),
        "profile": profile,
        "serial": serial,
        "dev_eui": dev_eui,
        // See the doc comment: a blank field rides as the serial.
        "name": name,
        "app_key": *app_key,
    });
    // Absent, not blank, when the operator cleared the field: absence is what
    // means "the Hub default" on the wire, for this agent and an older one.
    if let (Some(join_eui), Some(map)) = (join_eui.clone(), params.as_object_mut()) {
        map.insert("join_eui".to_string(), json!(join_eui));
    }
    // Refused before anything was signed: on the list's flash like every check above, with the
    // reason — an unpaired Hub above all — rather than "could not be registered" (#109). Not
    // audited, nothing was sent.
    let outcome = match cmd.execute_or_code(&hostname, params, None).await {
        Ok(outcome) => outcome,
        Err(code) => return Redirect::to(&format!("/nodes?error={code}")).into_response(),
    };

    let success = super::command::succeeded(&outcome);
    // Portal-side metadata, written only once the Hub took the device —
    // see the doc comment. A failure here is not a failed registration, so
    // it is logged and the operator can type the description again on the
    // detail page.
    let mut description_saved = false;
    if success {
        // The board's default bands, switched off (threshold_defaults.md §3). The Hub's
        // inventory document writes the same, a moment later; this one does not wait for it.
        super::seed_threshold_defaults(
            &state,
            &hostname,
            THRESHOLD_SCOPE_NODE,
            &dev_eui,
            render::BOARD_FIELDS,
        )
        .await;
        if let Some(text) = description.clone() {
            match super::edit_device_meta(
                &state,
                &current_user,
                &hostname,
                THRESHOLD_SCOPE_NODE,
                &dev_eui,
                |row| row.description = Some(text),
            )
            .await
            {
                Ok(()) => description_saved = true,
                Err(err) => {
                    tracing::warn!(%err, hostname, dev_eui, "node registered but its description was not stored")
                }
            }
        }
    }
    cmd.audit(
        "node_add",
        &dev_eui,
        // No `app_key`. Deliberately, and asserted by a test.
        json!({
            "serial": serial,
            "name": name,
            // Whether the note the operator typed actually landed in
            // `device_meta`, not whether they typed one — the same claim
            // `beacon_add` makes about the serial it stores.
            "description_recorded": description_saved,
            "profile": profile,
            // `null` = the operator left the Hub default (#86), or a P2P registration.
            "join_eui": join_eui,
            // The carrier the registration went out on (PU-1: this was the literal
            // `lorawan`, so a P2P registration was audited as LoRaWAN).
            "transport": carrier.as_str(),
        }),
        &outcome,
    )
    .await;

    if success {
        Redirect::to("/nodes?ok=registered").into_response()
    } else {
        // On the list page's own flash: the dialog is a plain form POST, so a
        // fragment would replace the whole page.
        let reason = add_failure_reason(&outcome);
        nodes_page_with(
            &state,
            &current_user,
            NodeListQuery::default(),
            Some(reason),
        )
        .await
    }
}

/// What the Portal keeps about a Node the Hub has just removed, dropped: its `device_meta`, its
/// alarm layout and delivery state, and its pending draft. A DevEUI is reused, so each of these
/// would otherwise land on the next device registered at it. Shared by [`remove_node`] and the
/// bulk removal of "Změnit rádio Hubu" (node_registry.md §12).
pub(crate) async fn forget_removed_node(state: &AppState, hostname: &str, node_key: &str) {
    super::forget_device_meta(state, hostname, THRESHOLD_SCOPE_NODE, node_key).await;
    // Its calibration reminder goes too; the certificates stay
    // (calibration.cs.md §4) — see `CalibrationService::forget_device`.
    state
        .calibration
        .forget_device(&crate::calibration::DeviceKey::new(
            hostname,
            crate::calibration::CalibrationScope::Node,
            node_key,
        ))
        .await;
    {
        let (h, n) = (hostname.to_string(), node_key.to_string());
        if let Err(err) = state
            .db
            .write(move |s| {
                s.delete_node_alarm_layout(&h, &n)?;
                // #120: a Node registered later under this key starts with every sensor on.
                s.delete_disabled_sensors_for_entity(&h, THRESHOLD_SCOPE_NODE, &n)?;
                // PF-3: what the Node confirmed of its table goes with it.
                s.delete_node_delivery(&h, &n).map(|_| ())
            })
            .await
        {
            tracing::warn!(%err, node_key, "node removed but its alarm layout survived");
        }
    }
    state
        .store
        .forget_disabled_sensors(hostname, THRESHOLD_SCOPE_NODE, node_key);
    match state.pending.discard(hostname, node_key).await {
        Ok(n) if n > 0 => {
            tracing::info!(node_key, rows = n, "node removed; its draft went with it")
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(%err, node_key, "node removed but its pending draft survived")
        }
    }
}

/// Removes a Node (devices.node.md §6).
///
/// Two things follow a successful removal, both because the row is keyed by
/// `(hostname, scope, entity)` and a DevEUI is reused: `device_meta` goes
/// (§19's F7 — otherwise the next device registered at that DevEUI inherits
/// a description and an offline override nobody chose for it) and the
/// pending draft goes with it (same argument, and a draft aimed at a device
/// that is gone is worse: it would be sent to its replacement).
///
/// **The wire verb is the v2 `node-remove`**, the signed twin of
/// `proximosctl control.radio node-remove --dev-eui`, for the same reason
/// `add_node` sends `node-add`. The agent deletes the device in ChirpStack,
/// publishes the retained tombstones the inventory reacts to, then drops
/// its record, in that order, and a failure anywhere is the command's
/// failure, not a "removed locally" success (lorawan.md open q13).
///
/// **Success is reported with the Hub's own message** (decision
/// 2026-09-07): the message is shown rather than swallowed.
async fn remove_node(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
    Form(form): Form<RemoveNodeForm>,
) -> Response {
    const COMMAND: &str = "node-remove";
    let cmd = Cmd::new(&state, &current_user, COMMAND);
    let node_key = node_key.to_lowercase();

    let Some((hostname, carrier)) = hub_and_carrier(&state, &node_key) else {
        return Redirect::to("/nodes?error=no_device").into_response();
    };
    if form.confirm.trim().to_lowercase() != node_key {
        super::audit_action(
            &state,
            &current_user,
            "node_remove",
            &node_key,
            json!({ "refused": "confirmation_mismatch" }),
            false,
        )
        .await;
        return Redirect::to(&format!(
            "/nodes/{node_key}/overview?error=confirm_mismatch"
        ))
        .into_response();
    }

    // `carrier` is the one the registry holds the Node on, not a constant (a P2P Node removed
    // as `lorawan` was refused by the Hub's mode gate, PN-8). One registry holds a Node on one
    // carrier (node_registry.md), so one removal is all of it.
    let outcome = match cmd
        .execute_or_code(
            &hostname,
            json!({ "radio": carrier.as_str(), "dev_eui": node_key }),
            None,
        )
        .await
    {
        Ok(outcome) => outcome,
        // Refused on this side before anything was signed, so nothing was sent; the code says
        // why — an unpaired Hub above all (#109).
        Err(code) => {
            return Redirect::to(&format!("/nodes/{node_key}/overview?error={code}"))
                .into_response()
        }
    };
    let success = super::command::succeeded(&outcome);

    if success {
        forget_removed_node(&state, &hostname, &node_key).await;
    }

    cmd.audit(
        "node_remove",
        &node_key,
        json!({
            "message": outcome.message,
            // The carrier the removal went to (PN-8).
            "transport": carrier.as_str(),
        }),
        &outcome,
    )
    .await;

    if success {
        Redirect::to("/nodes?ok=removed").into_response()
    } else {
        cmd.done(outcome)
    }
}

/// "Změnit rádio": moves a Node to the other carrier (node_registry.md §4, §6; devices.node.md,
/// dodatek 2026-09-27).
///
/// **Admin only, with a typed confirm** (the DevEUI), like removal: until the Node is switched
/// too it cannot reach the Hub. The confirm is checked here, and a mismatch is audited as a
/// refusal, as removal's is.
///
/// **The wire verb is the signed `node-move` `{dev_eui, to}`** (control.radio.md §7, amendment
/// 2026-09-27), through the same [`Cmd`] path `node-remove` takes. The Hub runs the CLI's own
/// `node-move`: the old carrier lets go, the registry flips, the retained `info` is republished.
/// **The Node itself is not told** (§10 Q1) — the dialog and the page after it say so.
///
/// **The audit row is `node_move` with `from` and `to`**, written for the Hub's answer whatever
/// it was; a refusal before anything was signed is not audited, as [`Cmd::relay`] does.
///
/// An htmx POST: a refusal, the Hub's included, renders into `#command-status` with its own
/// sentence, and success answers `HX-Redirect` to the settings tab, which then shows the new
/// carrier from the registry.
async fn move_node(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
    Form(form): Form<MoveNodeForm>,
) -> Response {
    const COMMAND: &str = "node-move";
    let cmd = Cmd::new(&state, &current_user, COMMAND);
    let node_key = node_key.to_lowercase();

    let Some((hostname, from)) = hub_and_carrier(&state, &node_key) else {
        return cmd.fail(&tr("monitoring.shared.no_device_yet"));
    };
    let Some(to) = carrier_named(&form.to) else {
        return cmd.fail(&tr("monitoring.node_detail.err_move_target"));
    };
    if to == from {
        return cmd.fail(&tr("monitoring.node_detail.err_move_same_carrier").replace(
            "{carrier}",
            &tr(&format!("monitoring.transport.{}", to.as_str())),
        ));
    }
    // #470 H4: a move to P2P onto an address another P2P Node holds, refused before anything
    // is signed, naming both; the Hub's own `AddressCollision` renders its slug's sentence.
    if to == portal_domain::Transport::P2p {
        if let Some(collision) = state.radio.p2p_address_collision(&node_key) {
            return cmd.fail(&address_collision_sentence(
                &node_key,
                &collision.addr,
                &collision.holders,
            ));
        }
    }
    if form.confirm.trim().to_lowercase() != node_key {
        super::audit_action(
            &state,
            &current_user,
            "node_move",
            &node_key,
            json!({ "refused": "confirmation_mismatch", "from": from.as_str(), "to": to.as_str() }),
            false,
        )
        .await;
        return cmd.fail(&tr("monitoring.node_detail.err_move_confirm_mismatch"));
    }

    let outcome = match cmd
        .execute(
            &hostname,
            json!({ "dev_eui": node_key, "to": to.as_str() }),
            None,
        )
        .await
    {
        Ok(outcome) => outcome,
        // Refused on this side before anything was signed, so nothing was sent.
        Err(refused) => return cmd.done(refused),
    };
    cmd.audit(
        "node_move",
        &node_key,
        json!({
            "from": from.as_str(),
            "to": to.as_str(),
            "message": outcome.message,
        }),
        &outcome,
    )
    .await;

    if super::command::succeeded(&outcome) {
        let mut response = StatusCode::OK.into_response();
        if let Ok(target) =
            axum::http::HeaderValue::from_str(&format!("/nodes/{node_key}/settings?ok=moved"))
        {
            response.headers_mut().insert("HX-Redirect", target);
        }
        response
    } else {
        // The Hub's own sentence, verbatim: "ChirpStack is not reachable … nothing changed" is
        // the only one that says what to do.
        cmd.done(outcome)
    }
}

/// Reveals a stored key value (devices.node.md §3.7), audited per reveal.
///
/// **The portal stores no AppKey** (decision 2026-09-07, and a finding
/// rather than a choice): `src/keys/` and `keys_repo` hold *user signing*
/// keys only, and the Hub's retained `info` omits `app_key` deliberately
/// (`enroll.rs`'s `KeyDisclosure::Omitted`). So there is no AppKey here to
/// mask or to reveal — the keys section renders "•••• (uložen na Hubu)"
/// with a note, and this route serves only the values the portal actually
/// holds: the session fields the Hub does publish on `NodeState`
/// (`dev_addr`, `net_id`, `session_state`), which are `None` until P2P
/// ships anyway.
///
/// It exists now rather than with those values because the *audit* is the
/// point (§11 gap H): a reveal has to leave a trail whatever it reveals,
/// and building the trail after the values arrive is how it gets forgotten.
async fn reveal_node_key(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
) -> Response {
    let node_key = node_key.to_lowercase();

    let portal_state = state.store.snapshot();
    // Any Hub of the cluster, not the first one (devices.hub.md §12 E).
    let node = portal_state
        .devices
        .values()
        .find_map(|d| d.nodes.get(&node_key));
    let Some(node) = node else {
        return not_found();
    };

    let values = json!({
        "dev_addr": node.dev_addr,
        "net_id": node.net_id,
        "session_state": node.session_state,
    });
    let revealed: Vec<&str> = values
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, _)| k.as_str())
                .collect()
        })
        .unwrap_or_default();

    // The names of what was revealed, never the values — an audit row is
    // read by more people than the page it describes.
    super::audit_action(
        &state,
        &current_user,
        "node_key_reveal",
        &node_key,
        json!({ "revealed": revealed }),
        true,
    )
    .await;

    super::render_fragment(
        &state,
        "partials/node_keys_revealed.html",
        render::page_ctx(
            context! { node_key => node_key, values => values },
            Some(&current_user),
        ),
    )
}

// ---------------------------------------------------------------------
// POST /nodes/{node_key}/service/{force-send,send-msg,reboot}
// ---------------------------------------------------------------------

/// The most a force-send reading trails the uplink that carried the command down.
const FORCE_SEND_PAIR_SECS: i64 = 10;

/// How long a Force send waits for its reading: the one answer budget
/// ([`crate::pending::downlink_answer_budget`]), which a config send's ack waits too. The
/// reading is the Node's answer, and it comes when the command does.
///
/// Two intervals alone were exact for P2P and nothing more. A P2P downlink rides the
/// *second* uplink after the send (the first ACK announces it, p2p.md B4/D2), and the
/// reading trails that uplink by a second or two, so a send just after an uplink saw its
/// reading land at the deadline and the watch had already said "No response" (night test
/// 2026-09-26, PN-3).
fn force_send_wait_secs(interval_report_s: Option<i64>) -> i64 {
    let budget = crate::pending::downlink_answer_budget(
        interval_report_s.and_then(|r| u64::try_from(r).ok()),
    );
    i64::try_from(budget.as_secs()).unwrap_or(i64::MAX)
}

#[derive(Deserialize)]
struct ForceSendWatchQuery {
    since: i64,
    until: i64,
}

#[derive(Deserialize)]
struct ServiceMessageForm {
    /// The raw JSON object the operator typed. Not validated against the
    /// registry — that is the whole point of SendMsg.
    payload: String,
}

#[derive(Deserialize)]
struct RebootForm {
    /// The Node key, typed by hand. Checked **on the server**: a
    /// client-side `required` attribute is a suggestion, and this is a
    /// reboot.
    #[serde(default)]
    confirm: String,
}

/// Force send — `node-send cmd force-send` through the seam (nodes_phase1.cs.md WP6).
///
/// The firmware sends an extra fPort-2 uplink right after the downlink and answers nothing on
/// fPort 85 (HIL #432), so there is no ack to wait for. The answer is a watch that polls
/// [`service_force_send_watch`] until the first reading after the send, or the Node's answer
/// budget (the downlink itself waits for the Node to transmit: its next uplink on LoRaWAN, the
/// second on P2P).
async fn service_force_send(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
) -> Response {
    let node_key = node_key.to_lowercase();
    let render_watch = |ctx: render::ForceSendCtx| {
        render_page(
            &state,
            "partials/node_force_send.html",
            context! { watch => ctx },
        )
    };
    if hostname_for_known_node(&state, &node_key).is_none() {
        return render_watch(render::ForceSendCtx::error(
            &node_key,
            tr("monitoring.shared.no_device_yet"),
        ));
    }
    let since = now_unix();
    let wait = state
        .store
        .snapshot()
        .hub_of_node(&node_key)
        .and_then(|(device, _)| device.node_configs.get(&node_key).cloned())
        .and_then(|entry| entry.config.get("application.interval_report")?.as_i64());
    let wait = force_send_wait_secs(wait);
    match state.radio.force_send(&node_key).await {
        Ok(_) => {
            super::audit_action(
                &state,
                &current_user,
                "node_service_force_send",
                &node_key,
                json!({ "transport": "control.radio" }),
                true,
            )
            .await;
            render_watch(render::ForceSendCtx::at(
                &node_key,
                since,
                since + wait,
                None,
                since,
            ))
        }
        Err(e) => render_watch(render::ForceSendCtx::error(&node_key, e.to_string())),
    }
}

/// Where a Force send stands: two readings at or after `since`, at most
/// [`FORCE_SEND_PAIR_SECS`] apart (the uplink that carried the command, then the force-send
/// one), confirm it; past `until` without them it had no response.
async fn service_force_send_watch(
    RequireAdmin(_current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
    Query(q): Query<ForceSendWatchQuery>,
) -> Response {
    let node_key = node_key.to_lowercase();
    let reading = match hostname_for_known_node(&state, &node_key) {
        Some(hostname) => {
            let n = node_key.clone();
            state
                .db
                .run(move |s| s.list_raw_messages(&hostname, &n, Some(50)))
                .await
                .unwrap_or_default()
                .into_iter()
                // A reading: fPort 2, or a path that carries no port (P2P); not an alarm
                // (fPort 3) and not a command answer (fPort 85).
                .filter(|m| matches!(portal_storage::raw_message_port(&m.payload), Some(2) | None))
                .map(|m| m.received_at)
                .filter(|at| *at >= q.since)
                .collect::<Vec<_>>()
        }
        None => Vec::new(),
    };
    // The downlink rides the RX window of an ordinary uplink, and the force-send reading
    // follows it within seconds (+1.6 s on 0413, WP4 2026-09-24). The first reading after
    // the send is that ordinary uplink, which proves only that the command left; the pair is
    // the Node answering it.
    let mut after = reading;
    after.sort_unstable();
    let reading = after
        .windows(2)
        .find(|pair| pair[1] - pair[0] <= FORCE_SEND_PAIR_SECS)
        .map(|pair| pair[1]);
    render_page(
        &state,
        "partials/node_force_send.html",
        context! {
            watch => render::ForceSendCtx::at(&node_key, q.since, q.until, reading, now_unix()),
        },
    )
}

/// Send raw message — a `set-config` JSON object, sent at once, no draft
/// (nodes_phase1.cs.md WP6).
///
/// The card offers templates from the firmware registry for the keys the Configuration tab
/// does not edit. The portal refuses only the keys it owns or that are not the radio's
/// ([`render::raw_message_key_refused`]: alarm slots, `history_sensors`, ROM codes, the
/// LoRaWAN group, `vendor_reset_allow`). Types, ranges and LoRaWAN writability are
/// control-radio's check (the firmware registry snapshot), and the firmware's own refusal
/// comes back on the Node's answer. The audit row names the keys, never the values.
async fn service_send_msg(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
    Form(form): Form<ServiceMessageForm>,
) -> Response {
    const COMMAND: &str = "set_sticker_config";
    let cmd = Cmd::new(&state, &current_user, COMMAND);
    let node_key = node_key.to_lowercase();

    // The one thing still checked: it has to be a JSON object, because
    // `set_sticker_config`'s `config` field is one. That is a shape, not a
    // rule about content.
    let config: serde_json::Map<String, serde_json::Value> =
        match serde_json::from_str::<serde_json::Value>(form.payload.trim()) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => return cmd.fail(&tr("monitoring.service.send_msg_not_json")),
        };

    let refused: Vec<&str> = config
        .keys()
        .map(String::as_str)
        .filter(|k| render::raw_message_key_refused(k))
        .collect();
    if !refused.is_empty() {
        return cmd.fail(
            &tr("monitoring.service.raw_key_refused").replace("{keys}", &refused.join(", ")),
        );
    }
    if config.is_empty() {
        return cmd.fail(&tr("monitoring.service.send_msg_not_json"));
    }

    let keys: Vec<String> = config.keys().cloned().collect();
    cmd.radio(
        state
            .radio
            .set_config(
                &node_key,
                crate::radio::config_assignments(&config),
                false,
                false,
            )
            .await,
        &tr("monitoring.shared.config_send_queued"),
        "node_service_send_msg",
        &node_key,
        // The keys, not the values: a service message is free-form and the
        // trail should say what was addressed without becoming a place
        // secrets end up.
        json!({ "keys": keys, "raw": true, "transport": "control.radio" }),
    )
    .await
}

/// Reboot — `sticker_reboot`, behind a typed confirmation checked here.
///
/// The dialog asks the admin to type the Node key. That check runs on the
/// **server**: the browser's own `required`/`pattern` attributes are a
/// convenience for the person typing, not a gate, and anyone can POST this
/// URL directly. A mismatch publishes nothing.
async fn service_reboot(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
    Form(form): Form<RebootForm>,
) -> Response {
    const COMMAND: &str = "sticker_reboot";
    let cmd = Cmd::new(&state, &current_user, COMMAND);
    let node_key = node_key.to_lowercase();

    if form.confirm.trim().to_lowercase() != node_key {
        // Audited: a refused reboot is worth a trail entry — it is either a
        // typo or somebody trying the URL.
        super::audit_action(
            &state,
            &current_user,
            "node_service_reboot",
            &node_key,
            json!({ "refused": "confirmation_mismatch" }),
            false,
        )
        .await;
        return cmd.fail(&tr("monitoring.service.reboot_confirm_mismatch"));
    }

    // `node-send cmd reboot` through the seam; the legacy `sticker_reboot` relay sends
    // nothing on a v2 Hub.
    cmd.radio(
        state.radio.reboot(&node_key).await,
        &tr("monitoring.shared.reboot_queued"),
        "node_service_reboot",
        &node_key,
        json!({ "confirmed": true, "transport": "control.radio" }),
    )
    .await
}

// ---------------------------------------------------------------------
// POST /nodes/{node_key}/backfill, POST /nodes/{node_key}/backfill/{job_id}/cancel
// ---------------------------------------------------------------------

/// Shared with the Beacon's History tab (`beacon_routes`), whose form is this
/// one (plan/portal/beacon_history.md §7.1).
#[derive(Deserialize)]
pub(crate) struct BackfillForm {
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
    /// A quick choice instead of `from`/`to`: `1h`, `6h` or `24h` back from now
    /// (devices.node.md §3.9, amendment 2026-09-25).
    #[serde(default)]
    pub(crate) preset: Option<String>,
    /// A gap row's exact window, unix seconds, instead of `from`/`to`.
    #[serde(default)]
    from_unix: Option<i64>,
    #[serde(default)]
    to_unix: Option<i64>,
}

impl BackfillForm {
    /// The requested window: a preset, a gap's own window, or the typed `from`/`to`, read in
    /// the display zone `tz` (decisions.md #32). `Err` says which input could not be read.
    pub(crate) fn window(
        &self,
        now: i64,
        tz: &jiff::tz::TimeZone,
    ) -> Result<(i64, i64), &'static str> {
        if let Some(preset) = self.preset.as_deref().filter(|p| !p.is_empty()) {
            let hours = match preset {
                "1h" => 1,
                "6h" => 6,
                "24h" => 24,
                _ => return Err("invalid 'preset'"),
            };
            return Ok((now - hours * 3_600, now));
        }
        if let (Some(from), Some(to)) = (self.from_unix, self.to_unix) {
            return Ok((from, to));
        }
        backfill_window(&self.from, &self.to, tz)
    }

    /// Nothing was chosen at all — a bare POST, which the Beacon reads as
    /// "resume from what the Hub holds" (beacon_history.md §2.1).
    pub(crate) fn is_empty(&self) -> bool {
        self.preset.as_deref().is_none_or(str::is_empty)
            && self.from_unix.is_none()
            && self.to_unix.is_none()
            && self.from.trim().is_empty()
            && self.to.trim().is_empty()
    }
}

/// The same context `node_detail_page` and `sse::handle_backfill_nudge`
/// build — one partial, one contract, three producers. `gaps` rides along
/// because a job created or cancelled here changes which holes are still
/// open, and the partial renders both (B3, §6).
async fn render_jobs_partial(state: &AppState, node_key: &str, can_edit: bool) -> Response {
    let jobs = state
        .backfill
        .jobs_for(node_key, None)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, node_key, "failed to load backfill jobs");
            Vec::new()
        });
    let rows: Vec<_> = jobs.iter().map(backfill::job_ctx).collect();
    // A Node no Hub claims is the "not adopted yet" state; the jobs table
    // still renders, without its gap list.
    let (gaps, history_off) = match hostname_for_known_node(state, node_key) {
        Some(hostname) => (
            state
                .backfill
                .gaps_ctx(&state.store, &hostname, node_key)
                .await,
            backfill::node_keeps_no_history(&state.store, &hostname, node_key),
        ),
        None => (Vec::new(), false),
    };
    render_page(
        state,
        "partials/backfill_jobs.html",
        context! { node_key => node_key, can_edit => can_edit, jobs => rows, gaps => gaps, history_off => history_off },
    )
}

/// The backfill form's from/to as UTC seconds, read as wall-clock time in
/// `tz`, the display zone (decisions.md #32).
fn backfill_window(
    from: &str,
    to: &str,
    tz: &jiff::tz::TimeZone,
) -> Result<(i64, i64), &'static str> {
    let from = crate::display_tz::parse_local_in(from.trim(), tz).ok_or("invalid 'from' date")?;
    let to = crate::display_tz::parse_local_in(to.trim(), tz).ok_or("invalid 'to' date")?;
    Ok((from, to))
}

async fn request_backfill(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
    headers: axum::http::HeaderMap,
    Form(form): Form<BackfillForm>,
) -> Response {
    let node_key = node_key.to_lowercase();
    let htmx = headers.contains_key("hx-request");

    let Some(hostname) = hostname_for_known_node(&state, &node_key) else {
        return (StatusCode::NOT_FOUND, "no device").into_response();
    };
    let refuse = |status: StatusCode, plain: String, key: &str, days: Option<i64>| {
        let message = tr(key).replace("{days}", &days.map(|d| d.to_string()).unwrap_or_default());
        backfill_refused(
            &state,
            &current_user,
            &node_key,
            htmx,
            status,
            plain,
            &message,
        )
    };
    // Decision #18: no history from a Hub that is not paired. Before, the job waited
    // "pending" with no explanation and the backfill loop warned every 5 s (S37-6).
    if !state.backfill.hub_paired(&hostname).await {
        return refuse(
            StatusCode::CONFLICT,
            "hub not paired".to_string(),
            "monitoring.backfill.err_hub_not_paired",
            None,
        );
    }
    let (from_ts, to_ts) = match form.window(now_unix(), &crate::display_tz::current()) {
        Ok(window) => window,
        Err(plain) => {
            return refuse(
                StatusCode::BAD_REQUEST,
                plain.to_string(),
                "monitoring.backfill.err_bad_date",
                None,
            )
        }
    };

    // Non-2xx on failure, jobs table left untouched — see this module's
    // doc comment ("Backfill error rendering").
    match state
        .backfill
        .request(&hostname, &node_key, from_ts, to_ts, "manual")
        .await
    {
        Ok(job) => {
            super::audit_action(
                &state,
                &current_user,
                "backfill_request",
                &node_key,
                json!({
                    "job_id": job.job_id,
                    "from": from_ts,
                    "to": to_ts,
                    "origin": "manual",
                    "preset": form.preset,
                }),
                true,
            )
            .await;
            render_jobs_partial(
                &state,
                &node_key,
                render::can_edit_thresholds(&current_user),
            )
            .await
        }
        Err(err @ BackfillError::Overlap) => refuse(
            StatusCode::CONFLICT,
            err.to_string(),
            "monitoring.backfill.err_overlap",
            None,
        ),
        Err(err @ BackfillError::EmptyWindow) => refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            err.to_string(),
            "monitoring.backfill.err_empty_window",
            None,
        ),
        Err(err @ BackfillError::FutureWindow) => refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            err.to_string(),
            "monitoring.backfill.err_future_window",
            None,
        ),
        Err(err @ BackfillError::WindowTooWide(days)) => refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            err.to_string(),
            "monitoring.backfill.err_too_wide",
            Some(days),
        ),
        Err(BackfillError::Storage(err)) => {
            tracing::error!(%err, hostname, node_key, "backfill request storage failure");
            (StatusCode::INTERNAL_SERVER_ERROR, "request failed").into_response()
        }
    }
}

/// A refused backfill request (N36, history tests 2026-09-25). The form's target is the jobs
/// table, which must not be replaced by an alert (module doc, "Backfill error rendering"), and
/// htmx drops a non-2xx answer, so the refusal used to show nothing at all. An htmx request now
/// gets 200 with `HX-Reswap: none` — the table stays as it is — and the reason as an
/// out-of-band `#command-status`; anything else keeps the plain status and text.
pub(crate) fn backfill_refused(
    state: &AppState,
    current_user: &CurrentUser,
    target: &str,
    htmx: bool,
    status: StatusCode,
    plain: String,
    message: &str,
) -> Response {
    if !htmx {
        return (status, plain).into_response();
    }
    let cmd = Cmd::new(state, current_user, "backfill_request").on(target);
    let ctx = render::page_ctx(
        context! { cmd => cmd.ctx(cmd.error(message)), oob => true },
        Some(current_user),
    );
    let mut response = super::render_fragment(state, "partials/command_status.html", ctx);
    response
        .headers_mut()
        .insert("hx-reswap", axum::http::HeaderValue::from_static("none"));
    response
}

async fn cancel_backfill(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path((node_key, job_id)): Path<(String, i64)>,
) -> Response {
    let node_key = node_key.to_lowercase();
    let cancelled = state.backfill.cancel(&node_key, job_id).await;
    if let Err(err) = &cancelled {
        tracing::warn!(%err, job_id, node_key, "backfill cancel failed");
    }
    super::audit_action(
        &state,
        &current_user,
        "backfill_cancel",
        &node_key,
        json!({ "job_id": job_id }),
        // `Ok(false)` is "no such job", not a cancellation — an audit trail
        // that records it as success is worse than one that records nothing.
        matches!(cancelled, Ok(true)),
    )
    .await;
    render_jobs_partial(
        &state,
        &node_key,
        render::can_edit_thresholds(&current_user),
    )
    .await
}

/// Loads the per-pass extras every Node row needs: `device_meta`, stored
/// thresholds, draft sizes, the stored-reading index, plus B2's offline
/// verdict and the signal-quality floors off the state. The history fault is
/// not among them: `backfill.rs` keeps it on `NodeState` (§19 F12, #64).
///
/// **Four database queries for the whole fleet, run concurrently** — see
/// [`render::NodeRowData`] for why none of them is done per row. A read
/// failure degrades that one input to empty rather than failing the page: a
/// Node list missing its descriptions is worth serving, a 500 is not.
pub(crate) async fn node_row_data(state: &AppState, hostname: &str) -> render::NodeRowData {
    let signal_bands = state.signal_quality;
    let (h1, h2, h3, h4) = (
        hostname.to_string(),
        hostname.to_string(),
        hostname.to_string(),
        hostname.to_string(),
    );
    let (meta, thresholds, pending_counts, with_readings) = tokio::join!(
        state
            .db
            .run(move |s| s.device_meta_map(&h1, THRESHOLD_SCOPE_NODE)),
        state
            .db
            .run(move |s| s.field_thresholds_by_entity(&h2, THRESHOLD_SCOPE_NODE)),
        state.db.run(move |s| s.count_pending_by_node(&h3)),
        state.db.run(move |s| s.nodes_with_readings(&h4)),
    );

    render::NodeRowData {
        meta: meta.unwrap_or_else(|err| {
            tracing::warn!(%err, hostname, "node device_meta unreadable; rows render without it");
            Default::default()
        }),
        thresholds: thresholds.unwrap_or_else(|err| {
            tracing::warn!(%err, hostname, "node thresholds unreadable; rows render without them");
            Default::default()
        }),
        liveness: state.offline.liveness_snapshot(),
        signal_bands,
        pending_counts: pending_counts.unwrap_or_else(|err| {
            tracing::warn!(%err, hostname, "pending drafts unreadable; rows render without the badge");
            Default::default()
        }),
        // Empty on failure means "no stored readings", which makes a silent
        // Node read as waiting-for-join — the pre-§21 behaviour, and the
        // conservative direction: the badge says "we have never heard from
        // this device", which is exactly what a portal that cannot read its
        // own readings knows.
        nodes_with_readings: with_readings.unwrap_or_else(|err| {
            tracing::warn!(%err, hostname, "stored-reading index unreadable; rows render without it");
            Default::default()
        }),
        calibration: state
            .calibration
            .levels_for(hostname, crate::calibration::CalibrationScope::Node),
        home_gateways: home_gateways(state),
    }
}

/// What the Gateway column names a Node's home gateway by (devices.node.md §5, dodatek
/// 2026-10-13): the Server's P2P sessions, and every gateway EUI the portal can put a Hub to —
/// each Hub's own (`system/info.lorawan.gateway_eui`), the `radio/gateways` rows that name
/// theirs, and the Client registry's. One file read and three in-memory walks per pass.
/// The Diagnostics tab's alarm lines and message rows for one Node: its
/// `alarm_events` rows, the raw-message ring as uplinks and the radio commands
/// the audit log recorded for it as downlinks. Each degrades to empty.
async fn node_diagnostics(
    state: &AppState,
    hostname: &str,
    node_key: &str,
    raw_rows: &[portal_storage::NodeRawMessage],
) -> minijinja::Value {
    let (alarms, audit) = tokio::join!(
        ({
            let filter = portal_storage::AlarmListFilter {
                limit: 50,
                hostname: Some(hostname.to_string()),
                sensor_id_prefix: Some(portal_domain::node_sensor_prefix(node_key)),
                ..Default::default()
            };
            state.db.run(move |s| s.list_alarms(&filter))
        }),
        ({
            let filter = portal_storage::AuditFilter {
                search: Some(node_key.to_string()),
                limit: 100,
                ..Default::default()
            };
            state.db.run(move |s| s.list_audit(&filter))
        }),
    );
    let alarms = alarms.map(|(rows, _)| rows).unwrap_or_else(|err| {
        tracing::warn!(%err, node_key, "failed to load the Node's alarms");
        Vec::new()
    });
    let downlinks: Vec<_> = audit
        .map(|(rows, _)| rows)
        .unwrap_or_else(|err| {
            tracing::warn!(%err, node_key, "failed to load the Node's commands");
            Vec::new()
        })
        .iter()
        .filter(|e| e.target.as_deref() == Some(node_key))
        .filter_map(render::diag_downlink_ctx)
        .collect();
    let uplinks = raw_rows.iter().map(render::diag_uplink_ctx).collect();
    // The data rate and frequency the last uplink arrived on, when the Hub
    // reported them (`dr` / `frequency` in the stored message).
    let last: Option<serde_json::Value> = raw_rows
        .iter()
        .max_by_key(|r| r.received_at)
        .and_then(|r| serde_json::from_str(&r.payload).ok());
    let last_of = |k: &str| {
        last.as_ref()
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_i64())
    };
    context! {
        dr => last_of("dr"),
        frequency => last_of("frequency"),
        alarms => render::diag_alarm_events(&alarms, 50),
        messages => render::diag_messages(uplinks, downlinks),
    }
}

fn home_gateways(state: &AppState) -> render::HomeGateways {
    let mut hubs_by_eui = std::collections::HashMap::new();
    for client in state.cluster.list() {
        if let Some(eui) = client.gateway_eui {
            hubs_by_eui.insert(eui.to_ascii_lowercase(), client.hostname);
        }
    }
    if let Some(gateways) = state.store.snapshot_radio_gateways() {
        for row in gateways.report.gateways {
            if let Some(hub) = row.hostname.filter(|h| !h.trim().is_empty()) {
                hubs_by_eui.insert(row.gateway_eui.to_ascii_lowercase(), hub);
            }
        }
    }
    let snapshot = state.store.snapshot();
    for device in snapshot.devices.values() {
        if let Some(eui) = device
            .lorawan
            .as_ref()
            .and_then(|l| l.gateway_eui.as_deref())
        {
            hubs_by_eui.insert(eui.to_ascii_lowercase(), device.hostname.clone());
        }
    }
    drop(snapshot);
    render::HomeGateways {
        server: registration_server(state),
        p2p: state.radio.p2p_homes(),
        hubs_by_eui,
    }
}

// ---------------------------------------------------------------------
// GET /nodes/{node_key}/messages.json
// ---------------------------------------------------------------------

/// Downloads one Node's raw message ring as a JSON array
/// (devices.backend.md §4, gap C).
///
/// **Byte-faithful to what was stored**: each element is the payload exactly
/// as `node_raw_messages` holds it, re-parsed only so the response is one
/// JSON array rather than a stream of concatenated documents. A row whose
/// payload will not parse is emitted as a JSON string of the raw text rather
/// than dropped — the point of a raw download is that nothing is filtered on
/// the way out.
///
/// Session-authenticated like every read route, and audited: this is a bulk
/// export of device data, which is the category WP0.1 says leaves a trail.
async fn download_node_messages(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
) -> Response {
    let node_key = node_key.to_lowercase();
    let Some(hostname) = hostname_for_known_node(&state, &node_key) else {
        return not_found();
    };

    let rows = match ({
        let a0 = hostname.clone();
        let a1 = node_key.clone();
        state
            .db
            .run(move |s| s.list_raw_messages(&a0, &a1, Some(50)))
    })
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(%err, node_key, "raw message download failed");
            // A failed bulk export is exactly what an audit trail is read
            // for — a repeatedly failing download used to leave no evidence
            // it had even been attempted (§19, F13).
            super::audit_action(
                &state,
                &current_user,
                "node_messages_download",
                &node_key,
                json!({ "error": err.to_string() }),
                false,
            )
            .await;
            return (StatusCode::INTERNAL_SERVER_ERROR, "download failed").into_response();
        }
    };

    let payload: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            serde_json::from_str(&row.payload)
                .unwrap_or_else(|_| serde_json::Value::String(row.payload.clone()))
        })
        .collect();

    // Serialise BEFORE the audit row, so the row cannot claim *n* messages
    // over a body that carries none. The old `unwrap_or_else(|| "[]")`
    // fallback did exactly that: an empty array served under a successful
    // audit row for however many messages the query returned (§19, F13).
    // Every element came out of `serde_json::from_str` or is a String, so a
    // failure here is not reachable — which is the reason to fail loudly
    // rather than to serve a plausible lie.
    let body = match serde_json::to_string_pretty(&payload) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(%err, node_key, "raw message download failed to serialise");
            super::audit_action(
                &state,
                &current_user,
                "node_messages_download",
                &node_key,
                json!({ "messages": payload.len(), "error": err.to_string() }),
                false,
            )
            .await;
            return (StatusCode::INTERNAL_SERVER_ERROR, "download failed").into_response();
        }
    };

    super::audit_action(
        &state,
        &current_user,
        "node_messages_download",
        &node_key,
        json!({ "messages": payload.len() }),
        true,
    )
    .await;

    (
        [
            (header::CONTENT_TYPE, "application/json".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"node-{node_key}-messages.json\""),
            ),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// decisions.md #32: the backfill form's from/to are read in the display
    /// zone, like every other time a person types.
    #[test]
    fn the_backfill_window_is_read_in_the_display_zone() {
        let prague = jiff::tz::TimeZone::get("Europe/Prague").unwrap();
        let utc = |s: &str| s.parse::<jiff::Timestamp>().unwrap().as_second();
        assert_eq!(
            backfill_window("2026-09-26T07:00", "2026-09-26T08:30", &prague),
            Ok((utc("2026-09-26T05:00:00Z"), utc("2026-09-26T06:30:00Z")))
        );
        assert_eq!(
            backfill_window("yesterday", "2026-09-26T08:30", &prague),
            Err("invalid 'from' date")
        );
        assert_eq!(
            backfill_window("2026-09-26T07:00", "", &prague),
            Err("invalid 'to' date")
        );
    }

    // ---- B1: list ordering (devices.backend.md §4) -----------------------

    fn row(node_key: &str, created_at: Option<&str>, name: Option<&str>) -> render::NodeCardCtx {
        render::NodeCardCtx {
            hostname: None,
            node_key: node_key.to_string(),
            name: name.map(str::to_string),
            serial_number: None,
            location: None,
            last_seen: None,
            alarm_state: "Normal".into(),
            rssi: None,
            snr: None,
            battery: None,
            battery_label: None,
            connected: true,
            fields: Vec::new(),
            transport: portal_domain::Transport::Lorawan,
            session_state: None,
            gateways: Vec::new(),
            home_gateway: None,
            fault: None,
            fw_version: None,
            description: None,
            created_at: created_at.map(str::to_string),
            thresholds: Default::default(),
            threshold_fields: Vec::new(),
            signal: render::SignalLevel::Unknown,
            pending_count: 0,
            waiting_for_join: false,
            calibration: None,
        }
    }

    fn keys(rows: &[render::NodeCardCtx]) -> Vec<&str> {
        rows.iter().map(|r| r.node_key.as_str()).collect()
    }

    /// The documented default: registration time, newest first.
    #[test]
    fn the_default_sort_is_newest_registration_first() {
        let mut rows = vec![
            row("old", Some("2026-01-01 00:00:00"), None),
            row("new", Some("2026-03-01 00:00:00"), None),
            row("mid", Some("2026-02-01 00:00:00"), None),
        ];
        sort_nodes(&mut rows, NodeSort::default(), SortDir::default());
        assert_eq!(keys(&rows), ["new", "mid", "old"]);
    }

    /// Equal rows break the tie on `node_key`, which is unique and stable.
    /// Without it a re-render can shuffle equal rows and the list appears to
    /// jitter under SSE churn — which is a bug an operator reports as
    /// "the page keeps moving", not as a sorting problem.
    #[test]
    fn equal_rows_keep_a_stable_order() {
        let same = Some("2026-01-01 00:00:00");
        let mut a = vec![
            row("ccc", same, None),
            row("aaa", same, None),
            row("bbb", same, None),
        ];
        let mut b = vec![
            row("bbb", same, None),
            row("ccc", same, None),
            row("aaa", same, None),
        ];
        sort_nodes(&mut a, NodeSort::CreatedAt, SortDir::Desc);
        sort_nodes(&mut b, NodeSort::CreatedAt, SortDir::Desc);
        assert_eq!(
            keys(&a),
            keys(&b),
            "the same set must render in the same order"
        );
    }

    /// Sorting by signal puts the worst links first regardless of direction
    /// default, and `Unknown` lands last — an unmeasured link is not
    /// evidence of a bad one, and hoisting it would bury the real ones.
    #[test]
    fn sorting_by_signal_leads_with_the_worst_measured_links() {
        let mut rows = vec![
            row("unknown", None, None),
            row("excellent", None, None),
            row("critical", None, None),
            row("weak", None, None),
        ];
        rows[1].signal = render::SignalLevel::Excellent;
        rows[2].signal = render::SignalLevel::Critical;
        rows[3].signal = render::SignalLevel::Weak;

        sort_nodes(&mut rows, NodeSort::Signal, SortDir::Asc);
        assert_eq!(keys(&rows), ["critical", "weak", "excellent", "unknown"]);
    }

    /// A Node with no `device_meta` row sorts last under the default rather
    /// than first: no registration record is "unknown", not "newest".
    #[test]
    fn a_node_without_metadata_does_not_masquerade_as_the_newest() {
        let mut rows = vec![
            row("unregistered", None, None),
            row("registered", Some("2026-01-01 00:00:00"), None),
        ];
        sort_nodes(&mut rows, NodeSort::CreatedAt, SortDir::Desc);
        assert_eq!(keys(&rows), ["registered", "unregistered"]);
    }

    /// Name sort is case-insensitive and falls back to the key for an
    /// unnamed Node, so the column never renders an empty cell at the top.
    #[test]
    fn name_sort_is_case_insensitive_and_falls_back_to_the_key() {
        let mut rows = vec![
            row("zzz", None, Some("banana")),
            row("aaa", None, None),
            row("mmm", None, Some("Apple")),
        ];
        sort_nodes(&mut rows, NodeSort::Name, SortDir::Asc);
        assert_eq!(keys(&rows), ["aaa", "mmm", "zzz"]);
    }

    #[test]
    fn parse_optional_bound_blank_is_none() {
        assert_eq!(parse_optional_bound("", "x").unwrap(), None);
        assert_eq!(parse_optional_bound("  ", "x").unwrap(), None);
    }

    #[test]
    fn parse_optional_bound_numeric_parses() {
        assert_eq!(parse_optional_bound("-5.5", "x").unwrap(), Some(-5.5));
    }

    #[test]
    fn parse_optional_bound_garbage_errors() {
        assert!(parse_optional_bound("abc", "critical_low").is_err());
    }

    #[test]
    fn downsample_leaves_small_series_untouched() {
        let points: Vec<_> = (0..10)
            .map(|i| NodeChartPoint {
                ts: i,
                avg: None,
                min: None,
                max: None,
            })
            .collect();
        assert_eq!(downsample(points, 2000).len(), 10);
    }

    #[test]
    fn downsample_strides_over_cap() {
        let points: Vec<_> = (0..4000)
            .map(|i| NodeChartPoint {
                ts: i,
                avg: None,
                min: None,
                max: None,
            })
            .collect();
        let out = downsample(points, 2000);
        assert!(out.len() <= 2000);
        assert!(!out.is_empty());
        // Stride sampling keeps ts=0 first.
        assert_eq!(out[0].ts, 0);
    }

    #[test]
    fn coerce_config_value_bool_matrix() {
        assert_eq!(
            coerce_config_value("application.history_enable", "true"),
            serde_json::Value::Bool(true)
        );
        assert_eq!(
            coerce_config_value("application.history_enable", "FALSE"),
            serde_json::Value::Bool(false)
        );
        // Garbage falls through as a string, letting validate_sticker_config
        // produce "expected a boolean".
        assert_eq!(
            coerce_config_value("application.history_enable", "yes"),
            serde_json::Value::String("yes".to_string())
        );
    }

    #[test]
    fn coerce_config_value_uint_matrix() {
        assert_eq!(
            coerce_config_value("application.interval_report", "300"),
            serde_json::Value::from(300u64)
        );
        assert_eq!(
            coerce_config_value("application.interval_report", "not a number"),
            serde_json::Value::String("not a number".to_string())
        );
    }

    #[test]
    fn coerce_config_value_enum_and_hex_pass_through_as_strings() {
        assert_eq!(
            coerce_config_value("sensors.accel_motion_sensitivity", "medium"),
            serde_json::Value::String("medium".to_string())
        );
        assert_eq!(
            coerce_config_value("alarms.alarm_0", &"a".repeat(34)),
            serde_json::Value::String("a".repeat(34))
        );
    }

    #[test]
    fn coerce_config_value_unknown_key_falls_through_as_string_and_validation_catches_it() {
        let value = coerce_config_value("not.a.key", "1");
        assert_eq!(value, serde_json::Value::String("1".to_string()));
        let mut map = serde_json::Map::new();
        map.insert("not.a.key".to_string(), value);
        let err = fiber_protocol::validate_sticker_config(&map).unwrap_err();
        assert_eq!(err[0].reason, "unknown key");
    }

    /// End-to-end over a real store, a real database and the real router.
    mod integration {
        use std::time::Instant;

        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use tower::ServiceExt;

        use crate::auth::SESSION_COOKIE;
        use crate::render;
        use crate::routes::test_support::{login, test_state};
        use crate::state::events::{IngestEvent, NodeReadingMsg};

        fn reading(node_key: &str, temperature: f64) -> NodeReadingMsg {
            NodeReadingMsg {
                message_id: format!("m-{node_key}"),
                dev_eui: node_key.to_string(),
                serial: Some(format!("SN-{node_key}")),
                fields: [("temperature".to_string(), temperature)]
                    .into_iter()
                    .collect(),
                counters: Default::default(),
                events: Vec::new(),
                gateways: Vec::new(),
                rssi: Some(-72),
                snr: Some(8.25),
                fport: None,
                page_index: None,
                page_count: None,
                dr: None,
                received_at: "2026-01-13T14:30:00Z".to_string(),
            }
        }

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

        async fn meta_rows(state: &crate::http::AppState) -> Vec<portal_storage::DeviceMeta> {
            ({
                let a0 = "h1".to_string();
                let a1 = portal_storage::THRESHOLD_SCOPE_NODE.to_string();
                state.db.run(move |s| s.device_meta_map(&a0, &a1))
            })
            .await
            .expect("device_meta")
            .into_values()
            .collect()
        }

        async fn meta_audit_rows(state: &crate::http::AppState) -> usize {
            state
                .auth
                .audit_page(portal_storage::AuditFilter {
                    event_type: Some("node_meta_set".into()),
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
                .expect("audit page")
                .0
                .len()
        }

        /// A viewer cannot write metadata, and the refusal leaves no audit
        /// row: the role gate is an extractor, so it runs before the handler
        /// body that would have written one.
        #[tokio::test]
        async fn a_viewer_cannot_write_device_metadata_and_leaves_no_trail() {
            let (state, _dir) = test_state().await;
            let viewer = crate::routes::test_support::login_as_role(
                &state,
                crate::auth::Role::Viewer,
                "viewer1",
            )
            .await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11", 21.5)),
            });
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                "/nodes/aa11/meta",
                &viewer,
                "description=cold+room&offline_k=",
            )
            .await;

            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert!(meta_rows(&state).await.is_empty(), "nothing was written");
            assert_eq!(meta_audit_rows(&state).await, 0, "no audit row for a 403");
        }

        /// A manager's save round-trips, is audited, and the second save
        /// neither moves `created_at` — the list's default sort key — nor
        /// re-stamps `added_by`. `0` survives as `0`: it means "detection
        /// off for this Node", where blank means "inherit the YAML default".
        /// Decision #41: name and location are stored in the Portal and shown at once; the
        /// offline card's post (no name, no location) leaves them alone.
        #[tokio::test]
        async fn a_manager_renames_a_node_and_the_store_shows_it_at_once() {
            let (state, _dir) = test_state().await;
            let manager = crate::routes::test_support::login_as_role(
                &state,
                crate::auth::Role::Manager,
                "manager1",
            )
            .await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11", 21.5)),
            });
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                "/nodes/aa11/meta",
                &manager,
                "name=+Mraznice+&description=&location=Sklad+B",
            )
            .await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            let rows = meta_rows(&state).await;
            assert_eq!(rows[0].name.as_deref(), Some("Mraznice"));
            assert_eq!(rows[0].location.as_deref(), Some("Sklad B"));
            let node = state.store.snapshot_device("h1").unwrap().nodes["aa11"].clone();
            assert_eq!(node.name.as_deref(), Some("Mraznice"));
            assert_eq!(node.location.as_deref(), Some("Sklad B"));

            let response = post(&router, "/nodes/aa11/meta", &manager, "offline_k=3").await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            let rows = meta_rows(&state).await;
            assert_eq!(
                rows[0].name.as_deref(),
                Some("Mraznice"),
                "absent = unchanged"
            );
            assert_eq!(
                rows[0].location.as_deref(),
                Some("Sklad B"),
                "absent = unchanged"
            );

            let long = "x".repeat(65);
            let response = post(
                &router,
                "/nodes/aa11/meta",
                &manager,
                &format!("name={long}"),
            )
            .await;
            assert!(response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|l| l.ends_with("error=name_too_long")));
        }

        #[tokio::test]
        async fn a_manager_save_round_trips_and_keeps_the_registration_stamp() {
            let (state, _dir) = test_state().await;
            let manager = crate::routes::test_support::login_as_role(
                &state,
                crate::auth::Role::Manager,
                "manager1",
            )
            .await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11", 21.5)),
            });
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                "/nodes/aa11/meta",
                &manager,
                "description=cold+room&offline_k=0",
            )
            .await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER);

            let rows = meta_rows(&state).await;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].description.as_deref(), Some("cold room"));
            assert_eq!(
                rows[0].offline_k_override,
                Some(0),
                "0 is a real setting, not a missing one"
            );
            assert!(rows[0].backfill_auto);
            assert_eq!(rows[0].added_by.as_deref(), Some("manager1"));
            assert_eq!(meta_audit_rows(&state).await, 1, "the save is audited");
            let created_at = rows[0].created_at.clone();

            // Second save: the override goes back to "inherit". The automatic history
            // download is the History switch's now, so no meta save turns it off.
            let response = post(
                &router,
                "/nodes/aa11/meta",
                &manager,
                "description=&offline_k=",
            )
            .await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER);

            let rows = meta_rows(&state).await;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].description, None, "blank clears the description");
            assert_eq!(rows[0].offline_k_override, None, "blank means inherit");
            assert!(
                rows[0].backfill_auto,
                "a meta save keeps the History switch as it is"
            );
            assert_eq!(
                rows[0].created_at, created_at,
                "an edit must not reshuffle the list's default sort"
            );
            assert_eq!(
                rows[0].added_by.as_deref(),
                Some("manager1"),
                "added_by is the registrant, not the last editor"
            );
        }

        /// Garbage in the override is refused before it reaches the row: a
        /// save that cannot be parsed must not quietly write "inherit".
        #[tokio::test]
        async fn a_nonsense_override_is_refused_rather_than_silently_inherited() {
            let (state, _dir) = test_state().await;
            let manager = crate::routes::test_support::login_as_role(
                &state,
                crate::auth::Role::Manager,
                "manager1",
            )
            .await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11", 21.5)),
            });
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                "/nodes/aa11/meta",
                &manager,
                "description=x&offline_k=-1",
            )
            .await;

            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            assert_eq!(
                response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/nodes/aa11/settings?error=invalid_offline_k")
            );
            assert!(meta_rows(&state).await.is_empty(), "nothing was written");
        }

        /// §3.3a: the ring the JSON download reads is on the page too, with
        /// the last alarm message beside it — "none" when none has arrived,
        /// rather than an invented one.
        #[tokio::test]
        async fn the_diagnostics_tab_lists_the_message_ring_and_the_last_alarm() {
            let (state, dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11", 21.5)),
            });
            {
                let storage = portal_storage::Storage::open(&dir.path().join("portal.db"), None)
                    .expect("open storage");
                storage
                    .insert_raw_message("h1", "aa11", 1_768_314_600, r#"{"fields":{"t":1.5}}"#)
                    .expect("insert");
            }
            let router = super::super::router().with_state(state.clone());

            let html = body_string(get(&router, "/nodes/aa11/diagnostics", &token).await).await;
            assert!(
                html.contains("/nodes/aa11/messages.json"),
                "download link: {html}"
            );
            assert!(
                html.contains("1.5"),
                "the stored value reaches the row: {html}"
            );
            assert!(
                html.contains("No alarm messages."),
                "no event means the muted 'none', not a made-up alarm: {html}"
            );
            assert!(
                html.contains("/trends?s=node:aa11:"),
                "full-history link: {html}"
            );
        }

        /// N36 (history tests 2026-09-25): a refused backfill request says why. htmx drops a
        /// non-2xx answer, so from the form (HX-Request) the refusal comes back 200 with the jobs
        /// table left alone and the reason out of band in `#command-status`; a plain client keeps
        /// the status code.
        #[tokio::test]
        async fn a_refused_backfill_request_says_why() {
            let (state, _dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11", 21.5)),
            });
            let router = super::super::router().with_state(state.clone());
            let send = |htmx: bool| {
                let mut builder = Request::builder()
                    .method("POST")
                    .uri("/nodes/aa11/backfill")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
                if htmx {
                    builder = builder.header("HX-Request", "true");
                }
                router.clone().oneshot(
                    builder
                        .body(Body::from("from=2099-01-01T00:00&to=2099-01-01T01:00"))
                        .expect("request"),
                )
            };

            let response = send(true).await.expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response
                    .headers()
                    .get("hx-reswap")
                    .and_then(|v| v.to_str().ok()),
                Some("none"),
                "the jobs table is left alone"
            );
            let html = body_string(response).await;
            assert!(html.contains("hx-swap-oob=\"true\""), "{html}");
            assert!(html.contains("entirely in the past"), "{html}");

            let response = send(false).await.expect("response");
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        }

        /// N31 (night test): after a reconnect the draft banner and the thresholds table re-read
        /// themselves; a portal restart left an open page showing a dead walk as `sending`.
        #[tokio::test]
        async fn a_reconnect_makes_the_draft_banner_re_read_itself() {
            let (state, _dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11", 21.5)),
            });
            let router = super::super::router().with_state(state.clone());

            let html = body_string(get(&router, "/nodes/aa11/overview", &token).await).await;
            assert!(
                html.contains("hx-on::sse-open="),
                "the shell announces a reconnect"
            );
            let banner = html
                .split("id=\"pending-config-aa11\"")
                .nth(1)
                .map(|rest| rest.chars().take(400).collect::<String>())
                .expect("the banner shell");
            assert!(banner.contains("portal:sse-resync from:body"), "{banner}");
            assert!(banner.contains("hx-disinherit=\"*\""), "{banner}");
        }

        /// The Keys & sessions card is gone from Node detail (2026-10-15); its
        /// reveal route stays.
        #[tokio::test]
        async fn the_node_settings_tab_has_no_keys_card() {
            let (state, _dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11", 21.5)),
            });
            let router = super::super::router().with_state(state.clone());

            let html = body_string(get(&router, "/nodes/aa11/settings", &token).await).await;
            assert!(!html.contains("keys&#x2f;reveal") && !html.contains("keys/reveal"));
            assert!(!html.contains("node-keys-revealed"));
        }

        /// N18 (night test): the optional "Last Activity" column printed the payload's RFC 3339
        /// string verbatim; it reads like the Overview cell beside it.
        #[tokio::test]
        async fn the_last_activity_column_is_a_readable_time() {
            let (state, _dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11", 21.5)),
            });
            let router = super::super::router().with_state(state.clone());

            let html = body_string(get(&router, "/nodes", &token).await).await;
            let cell = html
                .split("<td data-column=\"seen\"")
                .nth(1)
                .and_then(|rest| rest.split("</td>").next())
                .expect("the seen cell");
            assert!(cell.contains("Jan 13, 14:30:00"), "{cell}");
            assert!(!cell.contains("2026-01-13T14:30:00Z"), "{cell}");
        }

        /// Every tab closes what it opens. One stray `</div>` in a card ends `<main>` early; the
        /// version footer then lands beside the page as a flex item and squeezes it to a
        /// sliver at phone width (found on Diagnostics, combined23).
        #[tokio::test]
        async fn every_tab_closes_the_divs_it_opens() {
            let (state, _dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11", 21.5)),
            });
            let router = super::super::router().with_state(state.clone());

            for tab in ["overview", "settings", "history", "diagnostics"] {
                let uri = format!("/nodes/aa11/{tab}");
                let html = body_string(get(&router, &uri, &token).await).await;
                let start = html.find("<main").expect("<main>");
                let end = html.find("</main>").expect("</main>");
                let main = &html[start..end];
                let (opens, closes) =
                    (main.matches("<div").count(), main.matches("</div>").count());
                assert_eq!(
                    opens, closes,
                    "{tab}: {opens} <div> against {closes} </div>"
                );
            }
        }

        /// The download is byte-faithful to the stored ring and leaves an
        /// audit row — it is a bulk export of device data, which is the
        /// category WP0.1 says leaves a trail.
        #[tokio::test]
        async fn the_message_download_is_faithful_and_audited() {
            let (state, dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;

            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11", 21.5)),
            });

            // Two stored rows: one well-formed, one that will not parse. The
            // second is the interesting case — a raw download must not
            // silently drop what it cannot read.
            {
                let storage = portal_storage::Storage::open(&dir.path().join("portal.db"), None)
                    .expect("open storage");
                storage
                    .insert_raw_message("h1", "aa11", 1_768_314_600, r#"{"fields":{"t":1.5}}"#)
                    .expect("insert parseable");
                storage
                    .insert_raw_message("h1", "aa11", 1_768_314_601, "not json")
                    .expect("insert garbage");
            }

            let router = super::super::router().with_state(state.clone());
            let response = get(&router, "/nodes/aa11/messages.json", &token).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some("application/json")
            );

            let parsed: serde_json::Value =
                serde_json::from_str(&body_string(response).await).expect("a JSON array");
            let items = parsed.as_array().expect("array");
            assert_eq!(items.len(), 2);
            assert!(
                items.iter().any(|i| i == &serde_json::json!("not json")),
                "the unparseable row must survive the export: {items:?}"
            );
            assert!(items
                .iter()
                .any(|i| i.pointer("/fields/t") == Some(&serde_json::json!(1.5))));

            let (rows, _total) = state
                .auth
                .audit_page(portal_storage::AuditFilter {
                    event_type: Some("node_messages_download".into()),
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
            assert_eq!(rows.len(), 1, "the export must leave an audit row");
            assert_eq!(rows[0].target.as_deref(), Some("aa11"));
            assert!(rows[0].success);
            let details: serde_json::Value =
                serde_json::from_str(rows[0].details.as_deref().expect("details")).expect("json");
            assert_eq!(
                details["messages"], 2,
                "the count the row claims is the count the body carries"
            );
        }

        // ---- §19 (F13): the row and the body cannot disagree -------------

        /// The old `to_string_pretty(...).unwrap_or_else(|| "[]")` could
        /// serve an empty array under an audit row claiming *n* messages.
        /// Serialisation now happens before the row is written, so the two
        /// are the same number by construction — this pins that on a
        /// non-empty export, which is the case the fallback would have lied
        /// about.
        #[tokio::test]
        async fn the_download_body_carries_every_message_its_audit_row_claims() {
            let (state, dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11", 21.5)),
            });
            {
                let storage = portal_storage::Storage::open(&dir.path().join("portal.db"), None)
                    .expect("open storage");
                for i in 0..5 {
                    storage
                        .insert_raw_message(
                            "h1",
                            "aa11",
                            1_768_314_600 + i,
                            &format!(r#"{{"seq":{i}}}"#),
                        )
                        .expect("insert");
                }
            }

            let router = super::super::router().with_state(state.clone());
            let response = get(&router, "/nodes/aa11/messages.json", &token).await;
            let body = body_string(response).await;
            let parsed: serde_json::Value = serde_json::from_str(&body).expect("a JSON array");
            assert_eq!(parsed.as_array().expect("array").len(), 5);
            assert_ne!(body.trim(), "[]");

            let (rows, _total) = state
                .auth
                .audit_page(portal_storage::AuditFilter {
                    event_type: Some("node_messages_download".into()),
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
            let details: serde_json::Value =
                serde_json::from_str(rows[0].details.as_deref().expect("details")).expect("json");
            assert_eq!(details["messages"], 5);
        }

        // ---- §19 (F7): a meta write needs a device to write about --------

        /// `pending_config` is keyed `(hostname, node_key, param_key)`, so a
        /// draft for a key the store has never seen is a row the next device
        /// registered under that key inherits — complete with a Send button
        /// that would push a value nobody drafted for it. The meta POST has
        /// refused this since §19/F7; the two draft routes now do too.
        #[tokio::test]
        async fn a_draft_for_an_unknown_node_is_refused_and_writes_nothing() {
            let (state, _dir) = test_state().await;
            let manager = crate::routes::test_support::login_as_role(
                &state,
                crate::auth::Role::Manager,
                "manager1",
            )
            .await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11", 21.5)),
            });
            let router = super::super::router().with_state(state.clone());

            for (path, body) in [
                (
                    "/nodes/bb22/config",
                    "key=application.interval_report&value=600",
                ),
                (
                    "/nodes/bb22/thresholds/temperature",
                    "critical_low=&warning_low=&warning_high=&critical_high=30",
                ),
            ] {
                let response = post(&router, path, &manager, body).await;
                assert_ne!(
                    response.status(),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "{path} answers the operator rather than exploding"
                );
                assert!(
                    state
                        .pending
                        .list("h1", "bb22")
                        .await
                        .expect("list bb22")
                        .is_empty(),
                    "{path} left no orphan draft behind"
                );
            }

            // The registered Node still drafts normally.
            post(
                &router,
                "/nodes/aa11/config",
                &manager,
                "key=application.interval_report&value=600",
            )
            .await;
            assert!(
                !state
                    .pending
                    .list("h1", "aa11")
                    .await
                    .expect("list aa11")
                    .is_empty(),
                "a known Node is unaffected"
            );
        }

        /// `device_meta` is keyed by `(hostname, scope, entity)`, so a write
        /// for a key the store does not know plants a row the *next* Node
        /// registered under that key inherits.
        #[tokio::test]
        async fn a_meta_post_for_an_unknown_node_is_404_and_writes_nothing() {
            let (state, _dir) = test_state().await;
            let manager = crate::routes::test_support::login_as_role(
                &state,
                crate::auth::Role::Manager,
                "manager1",
            )
            .await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11", 21.5)),
            });
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                "/nodes/bb22/meta",
                &manager,
                "description=ghost&offline_k=",
            )
            .await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert!(meta_rows(&state).await.is_empty());
            assert_eq!(meta_audit_rows(&state).await, 0);

            // The known Node is unaffected.
            let ok = post(
                &router,
                "/nodes/aa11/meta",
                &manager,
                "description=freezer&offline_k=",
            )
            .await;
            assert_eq!(ok.status(), StatusCode::SEE_OTHER);
            assert_eq!(meta_rows(&state).await.len(), 1);
        }

        /// Builds a 500-Node fleet in the store — the size devices.node.md
        /// §2.9 calls *typical*, not a stress case.
        async fn five_hundred_node_state() -> (crate::http::AppState, tempfile::TempDir) {
            let (state, dir) = test_state().await;
            for i in 0..500 {
                let node_key = format!("aa{i:04x}");
                state.store.apply(IngestEvent::NodeReading {
                    hostname: "h1".into(),
                    node_key: node_key.clone(),
                    payload: Box::new(reading(&node_key, 20.0 + f64::from(i % 10))),
                });
            }
            assert_eq!(
                state.store.snapshot().devices["h1"].nodes.len(),
                500,
                "the fixture must actually build 500 Nodes"
            );
            (state, dir)
        }

        /// §4's mandatory acceptance gate, on the half this stage owns: the
        /// per-render work must not scale with the number of Nodes.
        ///
        /// The naive implementation reads `device_meta` and `field_thresholds`
        /// per row — two database round trips per Node, a thousand per render
        /// at this size — and that is the specific failure the spec names,
        /// with the hybrid server-side-filter variant as its escape hatch.
        /// `render::NodeRowData` makes it two queries for the whole page.
        ///
        /// Timed over the data-access phase alone, deliberately. The template
        /// render is O(rows) by construction and belongs to the presentation
        /// work (#66); folding it in would mean asserting a wall-clock number
        /// dominated by something this stage does not control. The budget
        /// below is ~200x the observed cost, so it survives a loaded CI box
        /// while a per-row regression (seconds, not milliseconds) cannot hide
        /// under it.
        #[tokio::test]
        async fn the_row_data_for_five_hundred_nodes_is_two_queries_not_a_thousand() {
            let (state, _dir) = five_hundred_node_state().await;
            let snapshot = state.store.snapshot();
            let device = &snapshot.devices["h1"];

            let started = Instant::now();
            for _ in 0..5 {
                let rows = super::super::node_row_data(&state, "h1").await;
                let built: Vec<_> = device
                    .nodes
                    .values()
                    .map(|n| render::node_card_ctx(rows.inputs(n, device)))
                    .collect();
                assert_eq!(built.len(), 500);
            }
            let per_pass = started.elapsed() / 5;

            assert!(
                per_pass < std::time::Duration::from_millis(500),
                "row data for 500 Nodes took {per_pass:?} — §4's gate. The cause is \
                 almost certainly per-row database access; see render::NodeRowData."
            );
        }

        /// The other half: the page actually renders, and every one of the
        /// 500 rows reaches the HTML — a list that silently truncates would
        /// pass any timing assertion.
        ///
        /// The coarse time bound is a backstop for constant-factor blowups
        /// rather than a performance measurement, and it is the assertion
        /// that earned this gate its keep: it first failed at **4.8 s** per
        /// render, and none of that was the data access above. Debug builds
        /// rebuilt the whole i18n catalogue from disk on every `|t` lookup —
        /// three per row, twelve thousand file reads per page — which also
        /// meant the dev loop re-did all of it on every SSE batch. Fixed in
        /// `i18n::HOT_RELOAD_INTERVAL`; ~72 ms after.
        ///
        /// Kept deliberately loose because it shares a CPU with 450 other
        /// tests: the same render measures ~72 ms alone and ~1.1 s under a
        /// full parallel run, so anything tighter tests the load on the box.
        #[tokio::test]
        async fn a_five_hundred_node_list_renders_every_row() {
            let (state, _dir) = five_hundred_node_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state);

            // The first render pays for template compilation, which is not
            // what is being bounded here.
            assert_eq!(
                get(&router, "/nodes", &token).await.status(),
                StatusCode::OK
            );

            let started = Instant::now();
            let response = get(&router, "/nodes", &token).await;
            assert_eq!(response.status(), StatusCode::OK);
            let elapsed = started.elapsed();

            let body = body_string(response).await;
            assert_eq!(
                body.matches("id=\"node-h1-").count(),
                500,
                "every Node must reach the HTML — a truncated list passes a timing check"
            );

            assert!(
                elapsed < std::time::Duration::from_secs(4),
                "500-row render took {elapsed:?}. This bound is a backstop for a \
                 constant-factor blowup (see i18n::HOT_RELOAD_INTERVAL), not a \
                 performance measurement — if it fires, look for per-row work in \
                 the render path rather than tuning the number."
            );
        }

        /// Sorting is honoured from the URL, and a stale or hand-edited
        /// query renders the list rather than a 400 — a shared link must not
        /// rot into an error page.
        #[tokio::test]
        async fn the_list_query_survives_nonsense() {
            let (state, _dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state);

            for path in [
                "/nodes",
                "/nodes?sort=name&dir=asc",
                "/nodes?sort=signal",
                "/nodes?q=cold+room&gateway=gw-1&threshold=warning,critical&fault=1",
                // The vocabulary retired on 2026-09-01 (4). A bookmark
                // carrying it must still render the list.
                "/nodes?threshold=not_set,warning_high",
            ] {
                assert_eq!(
                    get(&router, path, &token).await.status(),
                    StatusCode::OK,
                    "{path}"
                );
            }
        }

        /// The three value states survive into the template context and the
        /// retired five-way vocabulary does not. A stale `warning_high`
        /// must not reach the client as a filter it cannot evaluate — it
        /// would match nothing and read as an empty fleet.
        #[test]
        fn the_threshold_filter_keeps_three_states_and_drops_the_retired_ones() {
            use super::super::parse_threshold_filter;
            use super::super::ThresholdFilter::{Critical, Normal, Warning};

            assert_eq!(
                parse_threshold_filter(Some("warning,critical")),
                vec![Warning, Critical]
            );
            assert_eq!(parse_threshold_filter(Some(" Normal ")), vec![Normal]);
            assert_eq!(
                parse_threshold_filter(Some("not_set,warning_high,low_critical")),
                vec![]
            );
            // A mixed bookmark keeps what still means something.
            assert_eq!(
                parse_threshold_filter(Some("not_set,warning")),
                vec![Warning]
            );
            assert_eq!(
                parse_threshold_filter(Some("warning,warning")),
                vec![Warning]
            );
            assert_eq!(parse_threshold_filter(None), vec![]);
            assert_eq!(parse_threshold_filter(Some("")), vec![]);
        }
    }

    // ---- Two Hubs in the store (devices.hub.md §12 E) --------------------

    mod multi_hub {
        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use tower::ServiceExt;

        use crate::auth::SESSION_COOKIE;
        use crate::routes::test_support::{login, login_as_role, test_state};
        use crate::state::events::{IngestEvent, NodeReadingMsg};

        fn reading(node_key: &str) -> NodeReadingMsg {
            NodeReadingMsg {
                message_id: format!("m-{node_key}"),
                dev_eui: node_key.to_string(),
                serial: Some(format!("SN-{node_key}")),
                fields: [("temperature".to_string(), 21.5)].into_iter().collect(),
                counters: Default::default(),
                events: Vec::new(),
                gateways: Vec::new(),
                rssi: Some(-72),
                snr: Some(8.25),
                fport: None,
                page_index: None,
                page_count: None,
                dr: None,
                received_at: "2026-01-13T14:30:00Z".to_string(),
            }
        }

        /// `/nodes` is one list over the whole fleet, and each row carries the
        /// Hub it belongs to so its SSE target stays addressable.
        #[tokio::test]
        async fn the_list_holds_every_hubs_nodes() {
            let (state, _dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11")),
            });
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h2".into(),
                node_key: "bb22".into(),
                payload: Box::new(reading("bb22")),
            });
            let router = super::super::router().with_state(state.clone());

            let response = router
                .oneshot(
                    Request::builder()
                        .uri("/nodes")
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

            assert!(html.contains(r#"id="node-h1-aa11""#), "h1's Node is listed");
            assert!(html.contains(r#"id="node-h2-bb22""#), "h2's Node is listed");
            assert!(
                html.contains("node-added-h1") && html.contains("node-added-h2"),
                "the tbody subscribes to both Hubs' registrations"
            );
        }

        /// The write lands on the Hub that owns the key, not on the first Hub
        /// in the `BTreeMap` — `h1` sorts first and holds no Node at all.
        #[tokio::test]
        async fn a_node_write_lands_on_the_owning_hub() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, crate::auth::Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "h1".into(),
                status: fiber_protocol::payloads::status::DeviceStatus::Online,
            });
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h2".into(),
                node_key: "bb22".into(),
                payload: Box::new(reading("bb22")),
            });
            let router = super::super::router().with_state(state.clone());

            let response = router
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/nodes/bb22/meta")
                        .header(header::COOKIE, format!("{SESSION_COOKIE}={manager}"))
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from("description=cold+room&offline_k="))
                        .unwrap(),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::SEE_OTHER);

            let scope = portal_storage::THRESHOLD_SCOPE_NODE.to_string();
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
            assert!(on_h2.contains_key("bb22"), "written under the owning Hub");
            assert!(on_h1.is_empty(), "nothing under the first Hub in the map");
        }

        /// A key no Hub knows is a 404, which is also what stops a stale tab
        /// from planting an orphan `device_meta` row.
        #[tokio::test]
        async fn an_unknown_key_is_not_found() {
            let (state, _dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(reading("aa11")),
            });
            let router = super::super::router().with_state(state.clone());

            let response = router
                .oneshot(
                    Request::builder()
                        .uri("/nodes/cc33/overview")
                        .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }

    // ---- B4: the draft, the service card and the lifecycle ---------------
    //
    // devices.backend.md §7, spec devices.node.md §3.5-§3.7, §5, §6.

    mod b4 {
        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use tower::ServiceExt;

        use crate::auth::{Role, SESSION_COOKIE};
        use crate::http::AppState;
        use crate::routes::test_support::{login, login_as_role, test_state};
        use crate::state::events::IngestEvent;
        use fiber_protocol::payloads::config_state::{ConfigStateMsg, LorawanSensorConfigEntry};
        use fiber_protocol::payloads::status::DeviceStatus;
        use portal_storage::{NewFieldThreshold, THRESHOLD_SCOPE_NODE};

        use super::super::tr;

        const NODE: &str = "70b3d5e050011234";
        const HOST: &str = "h1";

        /// A paired Hub, so `ensure_user_key` has a device CA to mint
        /// against. Every route that relays needs one — an unpaired Hub is
        /// a `KeyError::NotPaired` and the handler stops before publishing,
        /// which is correct behaviour and useless as a test fixture.
        async fn pair_hub(state: &AppState) {
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

        /// Plays the Hub's part of the signed-command handshake so a route
        /// test can observe what a *successful* command does.
        ///
        /// `test_state`'s relay has no `run` loop and nothing answering it,
        /// so every signed command it issues times out. Removal's whole
        /// contract is what happens on success — `device_meta` and the
        /// draft go with the device — so that path has to be driven rather
        /// than asserted around. The sequence is the relay's own
        /// (`execute_config_request_completes_the_full_handshake`):
        /// request -> challenge -> confirm -> response.
        async fn with_signed_success<F, Fut>(state: &AppState, act: F) -> axum::response::Response
        where
            F: FnOnce() -> Fut + Send + 'static,
            Fut: std::future::Future<Output = axum::response::Response> + Send,
        {
            with_signed_successes(state, 1, act).await
        }

        /// [`with_signed_success`] for a handler that sends `commands` signed commands in a
        /// row, each answered with success (a removal from both registries, PU-2).
        async fn with_signed_successes<F, Fut>(
            state: &AppState,
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

            for n in 0..commands {
                let request_id = wait_for_nth(state, "commands/config/request", n)
                    .await
                    .expect("the command reached the wire")["request_id"]
                    .as_str()
                    .expect("request_id")
                    .to_string();
                let challenge_id = format!("chal-{}", n + 1);

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
                wait_for_nth(state, "commands/config/confirm", n)
                    .await
                    .expect("the portal confirmed");

                relay_tx
                    .send(IngestEvent::ConfigResponse {
                        hostname: HOST.into(),
                        payload: fiber_protocol::payloads::config_response::ConfigResponseMsg {
                            challenge_id,
                            request_id: None,
                            status: "SUCCESS".into(),
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

        /// Polls the relay's tape for the `n`-th frame (from 0) on `suffix`. Real time,
        /// deliberately: the handshake crosses a `spawn_blocking` (the key mint), and a paused
        /// clock would run ahead of it.
        async fn wait_for_nth(
            state: &AppState,
            suffix: &str,
            n: usize,
        ) -> Option<serde_json::Value> {
            for _ in 0..600 {
                if let Some((_, body)) = state
                    .relay
                    .published()
                    .into_iter()
                    .filter(|(topic, _)| topic.ends_with(suffix))
                    .nth(n)
                {
                    return Some(body);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            None
        }

        /// The `node-add`/`node-remove` requests the relay published, by `params.radio`.
        fn published_radios(state: &AppState, command: &str) -> Vec<String> {
            state
                .relay
                .published()
                .into_iter()
                .filter(|(topic, body)| {
                    topic.ends_with("commands/config/request") && body["command_type"] == command
                })
                .map(|(_, body)| body["params"]["radio"].as_str().unwrap_or("").to_string())
                .collect()
        }

        /// The details of the last audit row of `event`, parsed.
        async fn audit_details(state: &AppState, event: &str) -> serde_json::Value {
            audit_rows(state)
                .await
                .into_iter()
                .find(|row| row.event_type == event)
                .and_then(|row| row.details)
                .and_then(|d| serde_json::from_str(&d).ok())
                .unwrap_or(serde_json::Value::Null)
        }

        /// Puts one Node into the store through the authoritative v1
        /// inventory topic, which is what makes it addressable at all.
        fn seed_node(state: &AppState, dev_eui: &str) {
            state.store.apply(IngestEvent::Status {
                hostname: HOST.into(),
                status: DeviceStatus::Online,
            });
            state.store.apply(IngestEvent::ConfigState {
                hostname: HOST.into(),
                payload: ConfigStateMsg {
                    lorawan_sensors: vec![LorawanSensorConfigEntry {
                        dev_eui: dev_eui.into(),
                        name: None,
                        serial_number: None,
                        location: None,
                        enabled: Some(true),
                        temp_critical_low: None,
                        temp_warning_low: None,
                        temp_warning_high: None,
                        temp_critical_high: None,
                        humidity_critical_low: None,
                        humidity_warning_low: None,
                        humidity_warning_high: None,
                        humidity_critical_high: None,
                        field_thresholds: None,
                    }],
                    ..Default::default()
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

        async fn body_text(response: axum::response::Response) -> String {
            let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
                .await
                .expect("body");
            String::from_utf8_lossy(&bytes).into_owned()
        }

        /// GitLab #134: a count's chart reads `node_counter_readings`, where the decoder's
        /// `counters` are stored; a measurement still reads `node_field_readings`.
        #[tokio::test]
        async fn a_count_chart_reads_the_stored_counters() {
            let (state, _dir) = test_state().await;
            let token = login_as_role(&state, Role::Viewer, "viewer1").await;
            seed_node(&state, NODE);
            let hostname = state
                .store
                .snapshot()
                .hub_of_node(NODE)
                .map(|(d, _)| d.hostname.clone())
                .expect("the Node's Hub");
            let now = super::super::now_unix();
            let rows = [(now - 120, 4), (now - 60, 0)].map(|(ts, value)| {
                portal_storage::NodeCounterReading {
                    hostname: hostname.clone(),
                    node_key: NODE.into(),
                    counter: "motion_count".into(),
                    ts,
                    value: Some(value),
                }
            });
            state
                .db
                .run(move |s| s.insert_counter_readings(&rows))
                .await
                .expect("insert");
            let router = super::super::router().with_state(state.clone());
            let response = get(
                &router,
                &format!("/api/v1/chart/node/{NODE}/motion_count?range=1h"),
                &token,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let json: serde_json::Value =
                serde_json::from_str(&body_text(response).await).expect("json");
            let avg: Vec<_> = json["points"]
                .as_array()
                .expect("points")
                .iter()
                .map(|p| p["avg"].clone())
                .collect();
            assert_eq!(avg, [serde_json::json!(4.0), serde_json::json!(0.0)]);
        }

        /// LE-13: an action the radio seam failed (here: no radio config) is audited as
        /// failed, with the seam's sentence, instead of leaving no trace.
        #[tokio::test]
        async fn a_failed_radio_action_is_audited_as_failed() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());
            post(
                &router,
                &format!("/nodes/{NODE}/refresh-config"),
                &manager,
                "",
            )
            .await;
            let rows = audit_rows(&state).await;
            let row = rows
                .iter()
                .find(|r| r.event_type == "node_config_refresh")
                .expect("an audit row for the failed refresh");
            assert!(!row.success);
            assert!(
                row.details.as_deref().unwrap_or("").contains("error"),
                "{row:?}"
            );
        }

        async fn audit_rows(state: &AppState) -> Vec<portal_storage::AuditEntry> {
            state
                .auth
                .audit_page(portal_storage::AuditFilter {
                    event_type: None,
                    username: None,
                    since: None,
                    until: None,
                    success: None,
                    search: None,
                    limit: 200,
                    offset: 0,
                    ..Default::default()
                })
                .await
                .expect("audit page")
                .0
        }

        /// decisions.md #32: the backfill form opens prefilled with the last
        /// 24 hours in the display zone, as the export card does. It sits on the
        /// History tab (§3.9).
        #[tokio::test]
        async fn the_backfill_form_is_prefilled_in_the_display_zone() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            let html =
                body_text(get(&router, &format!("/nodes/{NODE}/history"), &manager).await).await;
            // From/To are days on the cally calendar (ui_components.md §9): the
            // prefill is yesterday..today in the display zone, and "now" is the
            // display-zone time a To of today sends.
            let now = crate::render::now_unix();
            let near = |at: i64, f: &dyn Fn(String) -> String| {
                [at, at - 60]
                    .iter()
                    .any(|t| html.contains(&f(crate::display_tz::datetime_local(*t))))
            };
            assert!(
                near(now - 24 * 3600, &|v| format!("fd: '{}'", &v[..10])),
                "{html}"
            );
            assert!(near(now, &|v| format!("nowLocal: '{v}'")), "{html}");
            // The labels name the zone the values are read in, not a fixed "UTC"
            // (hardware test 2026-10-09: a Prague page said UTC and meant CEST).
            let zone = format!("({})</label>", crate::display_tz::name());
            assert_eq!(
                html.replace("&#x2f;", "/").matches(&zone).count(),
                2,
                "{html}"
            );
        }

        // ---- WP4.2: the draft ------------------------------------------

        /// devices.node.md §3.4 (dodatek 2026-09-24): bounds only for a quantity the Node
        /// measures. This Node has no config info and has reported no pressure, so pressure
        /// takes no bounds and nothing is drafted.
        #[tokio::test]
        async fn a_bound_for_a_quantity_the_node_does_not_measure_is_refused() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            let html = body_text(
                post(
                    &router,
                    &format!("/nodes/{NODE}/thresholds/pressure"),
                    &manager,
                    "critical_low=900&warning_low=&warning_high=&critical_high=1100&enabled=on",
                )
                .await,
            )
            .await;
            assert!(
                html.contains("pressure"),
                "the refusal names the field: {html}"
            );
            assert!(state
                .pending
                .list(HOST, NODE)
                .await
                .expect("draft")
                .is_empty());
        }

        /// threshold_defaults.md §5: the battery takes no band, and says why, even on a Node
        /// that reports it; nothing is drafted.
        #[tokio::test]
        async fn a_battery_threshold_is_refused_with_the_reason() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            state.store.apply(IngestEvent::NodeReading {
                hostname: HOST.into(),
                node_key: NODE.into(),
                payload: Box::new(crate::state::NodeReadingMsg {
                    message_id: "m-batt".into(),
                    dev_eui: NODE.into(),
                    serial: None,
                    fields: [("voltage".to_string(), 3.6)].into_iter().collect(),
                    counters: Default::default(),
                    events: Vec::new(),
                    gateways: Vec::new(),
                    rssi: None,
                    snr: None,
                    fport: None,
                    page_index: None,
                    page_count: None,
                    dr: None,
                    received_at: "2026-09-28T08:00:00Z".to_string(),
                }),
            });
            let router = super::super::router().with_state(state.clone());

            let html = body_text(
                post(
                    &router,
                    &format!("/nodes/{NODE}/thresholds/voltage"),
                    &manager,
                    "critical_low=2.8&warning_low=3.0&warning_high=&critical_high=&enabled=on",
                )
                .await,
            )
            .await;
            assert!(
                html.contains(&tr("monitoring.shared.battery_not_thresholdable")),
                "{html}"
            );
            assert!(state
                .pending
                .list(HOST, NODE)
                .await
                .expect("draft")
                .is_empty());
        }

        /// GitLab #120: switching a sensor off with a drafted band drafts the band's removal
        /// (the Node's table goes out on Send), parks the sensor and refuses bounds for it
        /// until it is on again.
        #[tokio::test]
        async fn switching_a_sensor_off_drafts_its_band_away() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());
            // A band the Hub and the Node hold.
            state
                .db
                .write(|s| {
                    s.upsert_field_threshold(
                        &portal_storage::NewFieldThreshold {
                            hostname: HOST.into(),
                            scope: THRESHOLD_SCOPE_NODE.into(),
                            entity: NODE.into(),
                            field: "temperature".into(),
                            critical_low: Some(0.0),
                            warning_low: Some(2.0),
                            warning_high: Some(6.0),
                            critical_high: Some(8.0),
                            enabled: true,
                        },
                        Some("tester"),
                    )
                })
                .await
                .expect("band");

            let response = post(
                &router,
                &format!("/nodes/{NODE}/sensors/temperature"),
                &manager,
                "enabled=false",
            )
            .await;
            assert_eq!(
                response
                    .headers()
                    .get("HX-Redirect")
                    .and_then(|v| v.to_str().ok()),
                Some(&*format!(
                    "/nodes/{NODE}/overview?ok=sensor_off_send.temperature"
                ))
            );
            assert!(state
                .store
                .sensor_disabled(HOST, THRESHOLD_SCOPE_NODE, NODE, "temperature"));
            let draft = state.pending.list(HOST, NODE).await.expect("draft");
            let critical_low = draft
                .iter()
                .find(|r| {
                    r.param_key
                        == crate::pending::threshold_param_key("temperature", "critical_low")
                })
                .and_then(|r| r.desired_value.clone());
            assert_eq!(critical_low.as_deref(), Some("null"), "{draft:?}");
            assert!(draft.iter().any(|r| r.param_key
                == crate::pending::threshold_enabled_param_key("temperature")
                && r.desired_value.as_deref() == Some("false")));

            let html = body_text(
                post(
                    &router,
                    &format!("/nodes/{NODE}/thresholds/temperature"),
                    &manager,
                    "critical_low=0&warning_low=&warning_high=&critical_high=8&enabled=on",
                )
                .await,
            )
            .await;
            assert!(
                html.contains(&tr("monitoring.sensor_switch.err_off")),
                "{html}"
            );

            let response = post(
                &router,
                &format!("/nodes/{NODE}/sensors/temperature"),
                &manager,
                "enabled=true",
            )
            .await;
            assert!(response.headers().get("HX-Redirect").is_some());
            assert!(!state
                .store
                .sensor_disabled(HOST, THRESHOLD_SCOPE_NODE, NODE, "temperature"));
        }

        /// A band that was only drafted goes with the draft: nothing is left to send.
        #[tokio::test]
        async fn switching_a_sensor_off_drops_a_drafted_band() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());
            post(
                &router,
                &format!("/nodes/{NODE}/thresholds/temperature"),
                &manager,
                "critical_low=0&warning_low=2&warning_high=6&critical_high=8&enabled=on",
            )
            .await;
            assert!(!state
                .pending
                .list(HOST, NODE)
                .await
                .expect("draft")
                .is_empty());
            let response = post(
                &router,
                &format!("/nodes/{NODE}/sensors/temperature"),
                &manager,
                "enabled=false",
            )
            .await;
            assert_eq!(
                response
                    .headers()
                    .get("HX-Redirect")
                    .and_then(|v| v.to_str().ok()),
                Some(&*format!(
                    "/nodes/{NODE}/overview?ok=sensor_off_send.temperature"
                ))
            );
            // Amended 2026-10-15 (Q20d): the band was stored at once and may
            // already wait at the Hub, so the switch-off is sent to the Node.
            let draft = state.pending.list(HOST, NODE).await.expect("draft");
            assert_eq!(
                draft
                    .iter()
                    .map(|r| (r.param_key.as_str(), r.desired_value.as_deref()))
                    .collect::<Vec<_>>(),
                [
                    ("threshold.temperature.critical_high", Some("null")),
                    ("threshold.temperature.critical_low", Some("null")),
                    ("threshold.temperature.enabled", Some("false")),
                ],
                "{draft:?}"
            );
        }

        /// A band only the portal holds (switched off, undrafted) goes at once: nothing is
        /// drafted for the Node.
        #[tokio::test]
        async fn switching_a_sensor_off_drops_a_portal_only_band_without_a_draft() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());
            post(
                &router,
                &format!("/nodes/{NODE}/thresholds/temperature"),
                &manager,
                "critical_low=&warning_low=2&warning_high=6&critical_high=",
            )
            .await;
            assert!(state
                .pending
                .list(HOST, NODE)
                .await
                .expect("draft")
                .is_empty());

            let response = post(
                &router,
                &format!("/nodes/{NODE}/sensors/temperature"),
                &manager,
                "enabled=false",
            )
            .await;
            assert_eq!(
                response
                    .headers()
                    .get("HX-Redirect")
                    .and_then(|v| v.to_str().ok()),
                Some(&*format!(
                    "/nodes/{NODE}/overview?ok=sensor_off.temperature"
                ))
            );
            assert!(state
                .pending
                .list(HOST, NODE)
                .await
                .expect("draft")
                .is_empty());
            let band = state
                .db
                .run(|s| s.get_field_threshold(HOST, THRESHOLD_SCOPE_NODE, NODE, "temperature"))
                .await
                .expect("read");
            assert!(band.is_none());
        }

        /// Decision #41: a Node's hysteresis is the portal's alone, so it is stored at once and
        /// never drafted; the bounds still are. Saving the default back keeps following it.
        #[tokio::test]
        async fn a_node_hysteresis_is_stored_not_drafted() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());
            let own = |state: AppState| async move {
                state
                    .db
                    .run(|s| {
                        s.field_threshold_hysteresis(
                            HOST,
                            THRESHOLD_SCOPE_NODE,
                            NODE,
                            "temperature",
                        )
                    })
                    .await
                    .expect("read")
            };

            post(
                &router,
                &format!("/nodes/{NODE}/thresholds/temperature"),
                &manager,
                "critical_low=&warning_low=2&warning_high=6&critical_high=8&enabled=on&hysteresis=0.5",
            )
            .await;
            assert_eq!(
                own(state.clone()).await,
                None,
                "the default stays the default"
            );
            let drafted = state.pending.list(HOST, NODE).await.expect("draft");
            assert!(!drafted.iter().any(|r| r.param_key.contains("hysteresis")));

            post(
                &router,
                &format!("/nodes/{NODE}/thresholds/temperature"),
                &manager,
                "critical_low=&warning_low=2&warning_high=6&critical_high=8&enabled=on&hysteresis=1,2",
            )
            .await;
            assert_eq!(own(state.clone()).await, Some(1.2));

            post(
                &router,
                &format!("/nodes/{NODE}/thresholds/temperature"),
                &manager,
                "critical_low=&warning_low=2&warning_high=6&critical_high=8&enabled=on&hysteresis=",
            )
            .await;
            assert_eq!(
                own(state.clone()).await,
                None,
                "blank is back to the default"
            );
        }

        /// devices.node.md §3.5: one draft holds both halves. The threshold
        /// edit lands as four bound rows and the interval as one config
        /// row, and they are one banner rather than two.
        #[tokio::test]
        async fn a_threshold_edit_and_an_interval_edit_land_as_one_draft() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            assert_eq!(
                post(
                    &router,
                    &format!("/nodes/{NODE}/thresholds/temperature"),
                    &manager,
                    "critical_low=&warning_low=2&warning_high=6&critical_high=8&enabled=on",
                )
                .await
                .status(),
                StatusCode::OK
            );
            assert_eq!(
                post(
                    &router,
                    &format!("/nodes/{NODE}/config"),
                    &manager,
                    "key=application.interval_report&value=600",
                )
                .await
                .status(),
                StatusCode::OK
            );

            let rows = state.pending.list(HOST, NODE).await.expect("draft");
            let keys: Vec<&str> = rows.iter().map(|r| r.param_key.as_str()).collect();
            assert_eq!(
                keys,
                [
                    "application.interval_report",
                    "threshold.temperature.critical_high",
                    "threshold.temperature.critical_low",
                ],
                "the critical bounds that change plus the interval, in one draft; the blank \
                 critical_low comes in as the band's open side (P-3), an unstored field is on \
                 already, so `enabled` is not a change (N17), and the warnings are stored, not \
                 drafted (amendment 2026-10-13)"
            );
            assert!(rows.iter().all(|r| r.author == "manager1"));

            // And the banner renders both halves.
            let html =
                body_text(get(&router, &format!("/nodes/{NODE}/pending"), &manager).await).await;
            assert!(html.contains(&format!("pending-config-{NODE}")));
            assert!(html.contains("application.interval_report"));
            assert!(html.contains("manager1"));
        }

        /// Hynek 2026-10-14: the GP pin keys are read-only in the portal; a draft POST for one
        /// is refused and drafts nothing.
        #[tokio::test]
        async fn a_gp_pin_config_post_is_refused() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());
            let body = body_text(
                post(
                    &router,
                    &format!("/nodes/{NODE}/config"),
                    &manager,
                    "key=sensors.cap_gpio_mode&value=pir",
                )
                .await,
            )
            .await;
            assert!(body.contains(r#"data-status="error""#), "refused: {body}");
            let rows = state.pending.list(HOST, NODE).await.unwrap_or_default();
            assert!(rows.is_empty(), "nothing drafted: {rows:?}");
        }

        /// N17 (combined23): an edit drafts what it changes and nothing else. Against the bounds
        /// the portal holds, a re-posted value is no row, a blanked one is a `null` row ("clear
        /// this bound" is a change), and a value put back drops the row an earlier edit drafted.
        #[tokio::test]
        async fn a_threshold_edit_drafts_only_what_changes() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            state
                .db
                .write(|s| {
                    s.upsert_field_threshold(
                        &NewFieldThreshold {
                            hostname: HOST.into(),
                            scope: THRESHOLD_SCOPE_NODE.into(),
                            entity: NODE.into(),
                            field: "temperature".into(),
                            critical_low: Some(5.0),
                            warning_low: None,
                            warning_high: Some(30.0),
                            critical_high: Some(35.0),
                            enabled: true,
                        },
                        Some("admin"),
                    )
                })
                .await
                .expect("held bounds");
            let router = super::super::router().with_state(state.clone());
            let path = format!("/nodes/{NODE}/thresholds/temperature");
            let draft = || async {
                state
                    .pending
                    .list(HOST, NODE)
                    .await
                    .expect("draft")
                    .into_iter()
                    .map(|r| (r.param_key, r.desired_value.unwrap_or_default()))
                    .collect::<Vec<_>>()
            };
            let pair = |k: &str, v: &str| (format!("threshold.temperature.{k}"), v.to_string());

            let form = "critical_low=&warning_low=&warning_high=30&critical_high=32&enabled=on";
            assert_eq!(
                post(&router, &path, &manager, form).await.status(),
                StatusCode::OK
            );
            assert_eq!(
                draft().await,
                [pair("critical_high", "32.0"), pair("critical_low", "-99.0")],
                "the moved bound and the cleared one, which comes back as the band's open side \
                 (P-3); warning_high, warning_low and enabled are what the portal holds"
            );

            let form = "critical_low=&warning_low=&warning_high=30&critical_high=35&enabled=on";
            post(&router, &path, &manager, form).await;
            assert_eq!(
                draft().await,
                [pair("critical_high", "35.0"), pair("critical_low", "-99.0")],
                "critical_high back at 35 stays drafted: the 32 may already wait at the \
                 Hub, and this send supersedes it (Q20b, amended 2026-10-15)"
            );

            let form = "critical_low=5&warning_low=&warning_high=30&critical_high=35&enabled=on";
            post(&router, &path, &manager, form).await;
            assert_eq!(
                draft().await,
                [pair("critical_high", "35.0"), pair("critical_low", "5.0")],
                "everything as held, but the earlier edits may wait at the Hub: the held \
                 values go once more and supersede them (Q20b)"
            );
            let html = body_text(post(&router, &path, &manager, form).await).await;
            assert!(html.contains("No bound changed"), "{html}");
        }

        /// N22 (night test): an inverted or collapsed band never reaches the draft. Equal
        /// critical bounds are what the firmware refuses (`hi > lo`); one bound alone is fine.
        #[tokio::test]
        async fn a_band_out_of_order_is_refused() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());
            let path = format!("/nodes/{NODE}/thresholds/temperature");

            for form in [
                "critical_low=25&warning_low=&warning_high=&critical_high=25&enabled=on",
                "critical_low=30&warning_low=&warning_high=&critical_high=20&enabled=on",
                "critical_low=5&warning_low=10&warning_high=8&critical_high=25&enabled=on",
                "critical_low=5&warning_low=&warning_high=30&critical_high=25&enabled=on",
                "critical_low=5&warning_low=2&warning_high=&critical_high=25&enabled=on",
            ] {
                let html = body_text(post(&router, &path, &manager, form).await).await;
                assert!(html.contains("Bounds must rise"), "{form}: {html}");
                assert!(
                    state
                        .pending
                        .list(HOST, NODE)
                        .await
                        .expect("draft")
                        .is_empty(),
                    "{form} drafted something"
                );
            }
            for form in [
                "critical_low=&warning_low=&warning_high=&critical_high=25&enabled=on",
                "critical_low=5&warning_low=5&warning_high=25&critical_high=25&enabled=on",
            ] {
                let html = body_text(post(&router, &path, &manager, form).await).await;
                assert!(!html.contains("Bounds must rise"), "{form}: {html}");
            }
        }

        /// P-3 (nodes_phase2.cs.md, dodatek 2026-09-25): one critical bound on a field the Node
        /// watches gets the other filled (−99 / +199 for temperature), so the Node's rule is a
        /// range; the draft carries the filled bound like any other.
        #[tokio::test]
        async fn a_one_sided_critical_band_gets_its_open_side_filled() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());
            let path = format!("/nodes/{NODE}/thresholds/temperature");
            post(
                &router,
                &path,
                &manager,
                "critical_low=&warning_low=&warning_high=&critical_high=25&enabled=on",
            )
            .await;
            let drafted: std::collections::BTreeMap<String, Option<String>> = state
                .pending
                .list(HOST, NODE)
                .await
                .expect("draft")
                .into_iter()
                .map(|r| (r.param_key, r.desired_value))
                .collect();
            let key = |bound| crate::pending::threshold_param_key("temperature", bound);
            assert_eq!(
                drafted.get(&key("critical_low")),
                Some(&Some("-99.0".into())),
                "{drafted:?}"
            );
            assert_eq!(
                drafted.get(&key("critical_high")),
                Some(&Some("25.0".into())),
                "{drafted:?}"
            );
        }

        #[test]
        fn the_open_side_suits_the_quantity() {
            assert_eq!(
                super::complete_critical_band("humidity", (Some(20.0), None, None, None)),
                (Some(20.0), None, None, Some(199.0))
            );
            assert_eq!(
                super::complete_critical_band("pressure", (Some(950.0), None, None, None)),
                (Some(950.0), None, None, Some(9_999.0))
            );
            assert_eq!(
                super::complete_critical_band("w1_2_temperature", (None, None, None, Some(8.0))),
                (Some(-99.0), None, None, Some(8.0))
            );
            assert_eq!(
                super::complete_critical_band("voltage", (Some(2.8), None, None, None)),
                (Some(2.8), None, None, None),
                "no rule on the Node, the portal grades one side"
            );
            assert_eq!(
                super::complete_critical_band("temperature", (None, Some(5.0), Some(25.0), None)),
                (None, Some(5.0), Some(25.0), None),
                "warning bounds alone stay as they are"
            );
        }

        fn report_history(state: &AppState, enabled: bool) {
            state.store.apply(IngestEvent::NodeDeviceConfig {
                hostname: HOST.into(),
                node_key: NODE.into(),
                payload: Some(Box::new(
                    serde_json::from_value(serde_json::json!({
                        "dev_eui": NODE,
                        "received_at": "2026-09-25T09:00:00Z",
                        "seq": 0,
                        "page_count": 1,
                        "config": {
                            "application.history_enable": enabled,
                            "sensors.cap_barometer": false,
                        },
                    }))
                    .expect("config document"),
                )),
            });
        }

        async fn drafted(state: &AppState) -> Vec<(String, String)> {
            let mut rows: Vec<(String, String)> = state
                .pending
                .list(HOST, NODE)
                .await
                .expect("draft")
                .into_iter()
                .map(|r| (r.param_key, r.desired_value.unwrap_or_default()))
                .collect();
            rows.sort();
            rows
        }

        async fn backfill_auto(state: &AppState) -> bool {
            let (a0, a1, a2) = (
                HOST.to_string(),
                THRESHOLD_SCOPE_NODE.to_string(),
                NODE.to_string(),
            );
            state
                .db
                .run(move |s| s.get_device_meta(&a0, &a1, &a2))
                .await
                .expect("meta")
                .map(|m| m.backfill_auto)
                .unwrap_or(true)
        }

        /// devices.node.md §3.9, amendment 2026-09-25: one switch. On a Node that keeps no
        /// history it drafts `history_enable` with the mask of what the Node measures and sets
        /// the automatic download; off undoes both; a Node already in the wanted state gets no
        /// draft, because the change would only cost it a restart.
        #[tokio::test]
        async fn the_history_switch_drafts_the_node_and_sets_the_download() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, crate::auth::Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            report_history(&state, false);
            let router = super::super::router().with_state(state.clone());
            let path = format!("/nodes/{NODE}/history");

            let html = body_text(post(&router, &path, &manager, "enabled=true").await).await;
            assert!(
                html.contains(&tr("monitoring.history_switch.saved_to_draft")),
                "{html}"
            );
            assert!(backfill_auto(&state).await);
            assert_eq!(
                drafted(&state).await,
                vec![
                    (
                        "application.history_channels".to_string(),
                        format!("\"0001{}\"", "ff".repeat(22))
                    ),
                    ("application.history_enable".to_string(), "true".to_string()),
                ],
                "the board's temperature and humidity"
            );

            // Off before the draft went out: the Node already keeps none, so nothing is left
            // to send and the draft goes.
            let html = body_text(post(&router, &path, &manager, "").await).await;
            assert!(
                html.contains(&tr("monitoring.history_switch.saved_off")),
                "{html}"
            );
            assert!(!backfill_auto(&state).await);
            assert!(drafted(&state).await.is_empty());

            // A Node that already keeps history: on is the download alone.
            report_history(&state, true);
            let html = body_text(post(&router, &path, &manager, "enabled=true").await).await;
            assert!(
                html.contains(&tr("monitoring.history_switch.saved_on")),
                "{html}"
            );
            assert!(backfill_auto(&state).await);
            assert!(drafted(&state).await.is_empty());

            // …and off drafts `history_enable = false`, with no mask.
            post(&router, &path, &manager, "").await;
            assert_eq!(
                drafted(&state).await,
                vec![(
                    "application.history_enable".to_string(),
                    "false".to_string()
                )]
            );

            let settings =
                body_text(get(&router, &format!("/nodes/{NODE}/settings"), &manager).await).await;
            assert!(settings.contains("id=\"device-config-node\""));
            assert!(settings.contains("name=\"history\""));
            assert!(
                !settings.contains("name=\"backfill_auto\""),
                "the metadata card no longer carries the switch"
            );
        }

        /// The Device configuration card (ui_components.md §6): one check drafts the changed
        /// interval, saves the offline override and starts the send; an interval equal to what
        /// the Node reports and an unchanged History switch draft nothing.
        #[tokio::test]
        async fn the_device_config_card_drafts_saves_and_sends() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, crate::auth::Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            report_history(&state, true);
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());
            let path = format!("/nodes/{NODE}/device-config");

            let html = body_text(
                post(
                    &router,
                    &path,
                    &manager,
                    "interval_report=600&offline_k=2&history=on",
                )
                .await,
            )
            .await;
            let drafts = drafted(&state).await;
            assert!(
                drafts.contains(&("application.interval_report".to_string(), "600".to_string())),
                "{drafts:?}"
            );
            assert!(
                !drafts
                    .iter()
                    .any(|(k, _)| k == "application.history_enable"),
                "the switch did not change: {drafts:?}"
            );
            assert!(
                html.contains(&tr("monitoring.device_config.saved_sending")),
                "the check sends at once: {html}"
            );
            // The send walk publishes the signed set_sticker_config with the new interval
            // (mock relay; nothing reaches a device).
            let mut sent = None;
            for _ in 0..200 {
                sent = state.relay.published().into_iter().find(|(topic, body)| {
                    topic.ends_with("commands/config/request")
                        && body["command_type"] == "set_sticker_config"
                });
                if sent.is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let (_, body) = sent.expect("set_sticker_config published");
            assert!(
                body.to_string()
                    .contains("\"application.interval_report\":600"),
                "{body}"
            );
            let (a0, a1, a2) = (
                HOST.to_string(),
                THRESHOLD_SCOPE_NODE.to_string(),
                NODE.to_string(),
            );
            let meta = state
                .db
                .run(move |s| s.get_device_meta(&a0, &a1, &a2))
                .await
                .expect("meta")
                .expect("row");
            assert_eq!(meta.offline_k_override, Some(2));

            // A bad offline value drafts nothing more.
            let html = body_text(post(&router, &path, &manager, "offline_k=x").await).await;
            assert!(
                html.contains(&tr("monitoring.device_config.err_offline_k")),
                "{html}"
            );
        }

        /// The History tab (§3.8b, amendment 2026-09-25): quick choices ask for the last hours,
        /// a gap's button for exactly its window, and the card is no longer on Diagnostics.
        #[tokio::test]
        async fn quick_choices_and_a_gap_ask_for_their_windows() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, crate::auth::Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            report_history(&state, false);
            let router = super::super::router().with_state(state.clone());
            let path = format!("/nodes/{NODE}/backfill");

            let now = super::now_unix();
            post(&router, &path, &manager, "preset=6h").await;
            let jobs = state.backfill.jobs_for(NODE, None).await.expect("jobs");
            assert_eq!(jobs.len(), 1);
            assert!((jobs[0].time_to - now).abs() <= 2, "up to now");
            assert_eq!(jobs[0].time_to - jobs[0].time_from, 6 * 3_600);

            let (from, to) = (now - 30 * 86_400, now - 29 * 86_400);
            post(
                &router,
                &path,
                &manager,
                &format!("from_unix={from}&to_unix={to}"),
            )
            .await;
            let jobs = state.backfill.jobs_for(NODE, None).await.expect("jobs");
            assert!(
                jobs.iter().any(|j| j.time_from == from && j.time_to == to),
                "{jobs:?}"
            );

            let refused = post(&router, &path, &manager, "preset=7y").await;
            assert_eq!(refused.status(), StatusCode::BAD_REQUEST);

            let history =
                body_text(get(&router, &format!("/nodes/{NODE}/history"), &manager).await).await;
            assert!(history.contains(&tr("monitoring.backfill.request_heading")));
            assert!(history.contains(&format!("id=\"backfill-jobs-{NODE}\"")));
            assert!(
                history.contains(&tr("monitoring.history_switch.node_keeps_none")),
                "a Node that keeps no history is said so where downloads are asked for"
            );
            let diagnostics =
                body_text(get(&router, &format!("/nodes/{NODE}/diagnostics"), &manager).await)
                    .await;
            assert!(!diagnostics.contains(&format!("id=\"backfill-jobs-{NODE}\"")));
        }

        /// Puts one threshold row in the state a timed-out Node step leaves it: the Hub and
        /// `field_thresholds` hold `sent`, the Node did not confirm, and the row remembers the
        /// bound and the band from before the send.
        async fn undelivered_bound(
            state: &AppState,
            bound: &str,
            sent: f64,
            previous: f64,
            band_before: &str,
        ) {
            let key = crate::pending::threshold_param_key("temperature", bound);
            state
                .pending
                .upsert(portal_storage::NewPendingConfig {
                    hostname: HOST.into(),
                    node_key: NODE.into(),
                    param_key: key.clone(),
                    kind: portal_storage::PENDING_KIND_THRESHOLD.into(),
                    desired_value: Some(format!("{sent:?}")),
                    author: "manager1".into(),
                })
                .await
                .expect("draft");
            let band_before = band_before.to_string();
            state
                .db
                .write(move |s| {
                    s.land_hub_threshold(
                        HOST,
                        NODE,
                        &portal_storage::HubLanding {
                            hub_rows: &[],
                            node_rows: &[(
                                key.clone(),
                                Some(format!("{sent:?}")),
                                format!("{previous:?}"),
                            )],
                            previous_band: Some(&band_before),
                            threshold: None,
                            actor: None,
                        },
                    )?;
                    s.set_pending_state(
                        HOST,
                        NODE,
                        &key,
                        None,
                        portal_storage::PENDING_STATE_UNDELIVERED,
                        Some("device did not confirm"),
                        false,
                    )
                    .map(|_| ())
                })
                .await
                .expect("undelivered");
        }

        /// PF-3 review, item 1: a Discard waits while the walk writes the Hub, and the draft
        /// stays. *Amended 2026-10-15 (Q20e):* item 3's refusal of an out-of-order revert is
        /// gone with the revert itself.
        #[tokio::test]
        async fn discard_waits_for_the_hub_write_and_reverts_nothing() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());
            undelivered_bound(
                &state,
                "critical_high",
                30.0,
                25.0,
                r#"{"critical_low":40.0,"warning_low":null,"warning_high":null,"critical_high":25.0,"enabled":true}"#,
            )
            .await;
            let path = format!("/nodes/{NODE}/pending/discard");

            let phase = state.pending.enter_hub_phase(HOST, NODE);
            let refused = post(&router, &path, &manager, "").await;
            assert_eq!(refused.status(), StatusCode::OK);
            assert_eq!(
                refused
                    .headers()
                    .get("hx-reswap")
                    .and_then(|v| v.to_str().ok()),
                Some("none"),
                "the banner stays"
            );
            // The text past any apostrophe, which the template escapes.
            let busy = tr("monitoring.pending.send_in_progress");
            let busy = busy.rsplit('\'').next().expect("text");
            assert!(body_text(refused).await.contains(busy));
            assert_eq!(
                state.pending.list(HOST, NODE).await.expect("draft").len(),
                1
            );
            drop(phase);

            // config_sync.md §13 Q20e: once the Hub write is done, Discard reverts nothing —
            // not even an out-of-order band from before the send; it only drops the draft.
            assert_eq!(
                post(&router, &path, &manager, "").await.status(),
                StatusCode::OK
            );
            assert!(state
                .pending
                .list(HOST, NODE)
                .await
                .expect("draft")
                .is_empty());
            assert!(
                state.relay.published().is_empty(),
                "Q20e: no revert goes to the Hub"
            );
        }

        /// PF-3 review, item 4: a bound the Hub already took, edited away and back to what the
        /// portal holds, is the undelivered value again — not the detour, and not dropped.
        #[tokio::test]
        async fn an_edit_back_to_the_held_bound_is_undelivered_again() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            state
                .db
                .write(|s| {
                    s.upsert_field_threshold(
                        &NewFieldThreshold {
                            hostname: HOST.into(),
                            scope: THRESHOLD_SCOPE_NODE.into(),
                            entity: NODE.into(),
                            field: "temperature".into(),
                            critical_low: Some(5.0),
                            warning_low: None,
                            warning_high: Some(30.0),
                            critical_high: Some(32.0),
                            enabled: true,
                        },
                        Some("admin"),
                    )
                })
                .await
                .expect("held bounds");
            undelivered_bound(
                &state,
                "critical_high",
                32.0,
                35.0,
                r#"{"critical_low":5.0,"warning_low":null,"warning_high":30.0,"critical_high":35.0,"enabled":true}"#,
            )
            .await;
            let router = super::super::router().with_state(state.clone());
            let path = format!("/nodes/{NODE}/thresholds/temperature");
            let row = || async {
                state
                    .pending
                    .list(HOST, NODE)
                    .await
                    .expect("draft")
                    .into_iter()
                    .map(|r| (r.param_key, r.desired_value.unwrap_or_default(), r.state))
                    .collect::<Vec<_>>()
            };
            let key = "threshold.temperature.critical_high".to_string();

            post(
                &router,
                &path,
                &manager,
                "critical_low=5&warning_low=&warning_high=30&critical_high=33&enabled=on",
            )
            .await;
            assert_eq!(
                row().await,
                [(key.clone(), "33.0".to_string(), "pending".to_string())]
            );
            post(
                &router,
                &path,
                &manager,
                "critical_low=5&warning_low=&warning_high=30&critical_high=32&enabled=on",
            )
            .await;
            assert_eq!(
                row().await,
                [(key, "32.0".to_string(), "pending".to_string())],
                "the 33 may wait at the Hub: 32 is sent again and supersedes it \
                 (Q20b, amended 2026-10-15)"
            );
        }

        /// PF-3b review, follow-up: the Node took a discarded 30 and `differs`, and the
        /// operator discarded the portal's re-draft too. Saving the held band in the form drafts
        /// its critical bounds (N17's exception), so one Send puts them back on the Node; with
        /// the Node in agreement the same save drafts nothing.
        #[tokio::test]
        async fn saving_the_held_band_of_a_differing_field_drafts_it() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            state
                .db
                .write(|s| {
                    s.upsert_field_threshold(
                        &NewFieldThreshold {
                            hostname: HOST.into(),
                            scope: THRESHOLD_SCOPE_NODE.into(),
                            entity: NODE.into(),
                            field: "temperature".into(),
                            critical_low: Some(-10.0),
                            warning_low: None,
                            warning_high: None,
                            critical_high: Some(25.0),
                            enabled: true,
                        },
                        Some("admin"),
                    )
                })
                .await
                .expect("held bounds");
            let router = super::super::router().with_state(state.clone());
            let path = format!("/nodes/{NODE}/thresholds/temperature");
            let form = "critical_low=-10&warning_low=&warning_high=&critical_high=25&enabled=on";
            let draft = || async {
                state
                    .pending
                    .list(HOST, NODE)
                    .await
                    .expect("draft")
                    .into_iter()
                    .map(|r| (r.param_key, r.desired_value.unwrap_or_default(), r.author))
                    .collect::<Vec<_>>()
            };

            post(&router, &path, &manager, form).await;
            assert!(
                draft().await.is_empty(),
                "N17: the held band drafts nothing"
            );

            let band = |high: f64| portal_storage::NodeBand {
                field: "temperature".into(),
                low: Some(-10.0),
                high: Some(high),
                enabled: true,
            };
            state
                .db
                .write(move |s| {
                    s.mark_node_delivery_pending(HOST, NODE, &[band(30.0)], 1)?;
                    s.mark_node_delivery_unknown(HOST, NODE, None)?;
                    s.node_took_discarded(HOST, NODE, 2, &[band(25.0)])
                })
                .await
                .expect("differs");
            assert!(state.pending.field_differs(HOST, NODE, "temperature").await);

            post(&router, &path, &manager, form).await;
            assert_eq!(
                draft().await,
                [
                    (
                        "threshold.temperature.critical_high".to_string(),
                        "25.0".to_string(),
                        "manager1".to_string()
                    ),
                    (
                        "threshold.temperature.critical_low".to_string(),
                        "-10.0".to_string(),
                        "manager1".to_string()
                    ),
                ],
                "the portal's band, drafted for one Send"
            );
        }

        /// config_sync.md §13 Q20e: Discard keeps the saved limit; the Node still holds the band
        /// it confirmed, so the row shows the "not synchronised" badge (Node detail and
        /// `/limits`) and the banner. "Odeslat do Nodu" drafts the differing limit as one
        /// message; the Node's settled ack clears the badge.
        #[tokio::test]
        async fn discard_keeps_the_limit_and_the_node_band_is_flagged_until_resent() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, crate::auth::Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());
            state
                .db
                .write(|s| {
                    s.upsert_field_threshold(
                        &portal_storage::NewFieldThreshold {
                            hostname: HOST.into(),
                            scope: THRESHOLD_SCOPE_NODE.into(),
                            entity: NODE.into(),
                            field: "temperature".into(),
                            critical_low: Some(-10.0),
                            warning_low: None,
                            warning_high: None,
                            critical_high: Some(30.0),
                            enabled: true,
                        },
                        Some("manager1"),
                    )?;
                    s.confirm_node_delivery(
                        HOST,
                        NODE,
                        &[portal_storage::NodeBand {
                            field: "temperature".into(),
                            low: Some(-10.0),
                            high: Some(30.0),
                            enabled: true,
                        }],
                        1_790_000_000,
                    )
                })
                .await
                .expect("seed");
            let badge =
                tr("monitoring.threshold_table.node_unsynced_badge").replace("{band}", "-10 – 30");
            let detail = format!("/nodes/{NODE}/overview");

            // In sync: no badge, no banner.
            let page = body_text(get(&router, &detail, &manager).await).await;
            assert!(!page.contains("data-node-unsynced"));

            // Save 35, then discard the message.
            assert_eq!(
                post(
                    &router,
                    &format!("/nodes/{NODE}/thresholds/temperature"),
                    &manager,
                    "critical_low=-10&warning_low=&warning_high=&critical_high=35&enabled=on",
                )
                .await
                .status(),
                StatusCode::OK
            );
            assert!(!state
                .pending
                .list(HOST, NODE)
                .await
                .expect("draft")
                .is_empty());
            // While the message is pending, the badge stays away.
            let page = body_text(get(&router, &detail, &manager).await).await;
            assert!(!page.contains(&badge), "pending, not unsynchronised");
            assert_eq!(
                post(
                    &router,
                    &format!("/nodes/{NODE}/pending/discard"),
                    &manager,
                    ""
                )
                .await
                .status(),
                StatusCode::OK
            );
            let stored = state
                .db
                .run(|s| s.list_field_thresholds(HOST, THRESHOLD_SCOPE_NODE, NODE))
                .await
                .expect("stored");
            assert_eq!(
                stored[0].critical_high,
                Some(35.0),
                "Q20e: Discard keeps it"
            );
            assert!(state
                .pending
                .list(HOST, NODE)
                .await
                .expect("draft")
                .is_empty());

            let page = body_text(get(&router, &detail, &manager).await).await;
            assert!(page.contains("data-node-unsynced"), "the banner");
            assert!(page.contains(&format!("/nodes/{NODE}/limits/resync")));
            let limits = super::super::super::limits_routes::router().with_state(state.clone());
            let page = body_text(get(&limits, "/limits", &manager).await).await;
            assert!(page.contains(&badge), "the /limits row too");

            // "Odeslat do Nodu": one message carrying the differing limit.
            let rows_before_send = {
                let unsynced = super::super::unsynced_limits(&state, HOST, NODE).await;
                assert_eq!(unsynced.len(), 1);
                assert_eq!(unsynced[0].0, "temperature");
                unsynced
            };
            assert_eq!(rows_before_send[0].1.critical_high, Some(35.0));
            assert_eq!(
                post(
                    &router,
                    &format!("/nodes/{NODE}/limits/resync"),
                    &manager,
                    ""
                )
                .await
                .status(),
                StatusCode::OK
            );
            for _ in 0..200 {
                if !state.pending.is_walking(HOST, NODE) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let rows = state.pending.list(HOST, NODE).await.expect("draft");
            let mut keys: Vec<&str> = rows.iter().map(|r| r.param_key.as_str()).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                [
                    crate::pending::threshold_param_key("temperature", "critical_high").as_str(),
                    crate::pending::threshold_param_key("temperature", "critical_low").as_str(),
                    crate::pending::threshold_enabled_param_key("temperature").as_str(),
                ]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
            );
            let device = state.store.snapshot_device(HOST);
            let units = crate::pending::plan_send(
                &rows,
                NODE,
                &super::super::merged_thresholds_for(
                    &state,
                    HOST,
                    device.as_ref().and_then(|d| d.nodes.get(NODE)),
                    NODE,
                )
                .await,
                &super::super::node_alarm_layout(device.as_ref(), NODE),
                &crate::radio::alarm::SlotTypes::of_node(
                    device.as_ref().and_then(|d| d.node_configs.get(NODE)),
                ),
            );
            assert_eq!(
                units.iter().filter(|u| !u.node_bands.is_empty()).count(),
                1,
                "one message to the Node"
            );
            let page = body_text(get(&router, &detail, &manager).await).await;
            assert!(!page.contains(&badge), "pending again");

            // The Node's ack settles it: the badge goes.
            state
                .db
                .write(|s| {
                    s.confirm_node_delivery(
                        HOST,
                        NODE,
                        &[portal_storage::NodeBand {
                            field: "temperature".into(),
                            low: Some(-10.0),
                            high: Some(35.0),
                            enabled: true,
                        }],
                        1_790_000_100,
                    )
                })
                .await
                .expect("ack");
            state.pending.discard(HOST, NODE).await.expect("settled");
            let page = body_text(get(&router, &detail, &manager).await).await;
            assert!(!page.contains("data-node-unsynced"));
            assert!(!page.contains(&badge));
            assert!(super::super::unsynced_limits(&state, HOST, NODE)
                .await
                .is_empty());
        }

        /// F16 (Nodes test 2026-09-23): the draft's "on the device" column reads the Node's
        /// config document under the Hub's display spelling, not only when the Hub happens to
        /// be lowercase already.
        #[tokio::test]
        async fn the_draft_quotes_the_device_value_under_an_uppercase_hub_name() {
            const DISPLAY: &str = "FIBER-4E970F4A";
            let (state, _dir) = test_state().await;
            state.store.apply(IngestEvent::Status {
                hostname: DISPLAY.into(),
                status: DeviceStatus::Online,
            });
            state.store.apply(IngestEvent::NodeDeviceConfig {
                hostname: DISPLAY.to_ascii_lowercase(),
                node_key: NODE.into(),
                payload: Some(Box::new(
                    serde_json::from_value(serde_json::json!({
                        "dev_eui": NODE,
                        "received_at": "2026-09-23T23:01:03Z",
                        "seq": 30,
                        "page_count": 1,
                        "config": {"application.interval_sample": 30},
                    }))
                    .expect("config document"),
                )),
            });
            let hostname = state
                .store
                .snapshot_device(DISPLAY)
                .expect("the Hub")
                .hostname;
            // Decision #21: the store keeps the one lowercase spelling.
            assert_eq!(
                hostname,
                DISPLAY.to_ascii_lowercase(),
                "the display spelling is what routes use"
            );
            state
                .pending
                .upsert(portal_storage::NewPendingConfig {
                    hostname: hostname.clone(),
                    node_key: NODE.into(),
                    param_key: "application.interval_sample".into(),
                    kind: portal_storage::PENDING_KIND_CONFIG.into(),
                    desired_value: Some("45".into()),
                    author: "admin".into(),
                })
                .await
                .expect("draft");

            let ctx = super::pending_ctx_for(&state, &hostname, NODE, true).await;
            let row = ctx
                .rows
                .iter()
                .find(|r| r.param_key == "application.interval_sample")
                .expect("the drafted row");
            assert_eq!(row.current.as_deref(), Some("30"));
        }

        /// The point of the whole work package: an edit publishes nothing.
        #[tokio::test]
        async fn a_draft_edit_sends_nothing() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            post(
                &router,
                &format!("/nodes/{NODE}/config"),
                &manager,
                "key=application.interval_report&value=600",
            )
            .await;

            assert!(
                state.relay.published().is_empty(),
                "a draft write must not reach the relay: {:?}",
                state.relay.published()
            );
            let rows = audit_rows(&state).await;
            assert!(
                rows.iter().any(|r| r.event_type == "node_config_draft"),
                "the draft write is audited as a draft, not as a config set"
            );
            assert!(
                !rows.iter().any(|r| r.event_type == "node_config_set"),
                "nothing was set"
            );
        }

        /// The validation did not move with the relay call: a value the
        /// wire would reject is refused at the draft, not stored to fail
        /// three minutes later in front of the operator.
        #[tokio::test]
        async fn a_draft_still_refuses_a_value_the_wire_would_reject() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            // nodes_phase1.cs.md WP5: only the intervals, and only inside the firmware's range.
            for (form, reason) in [
                (
                    "key=nonsense.key&value=1",
                    "Only the report and sample intervals",
                ),
                (
                    "key=application.history_enable&value=true",
                    "Only the report and sample intervals",
                ),
                (
                    "key=application.interval_report&value=59",
                    "60\u{2013}86400",
                ),
                (
                    "key=application.interval_report&value=86401",
                    "60\u{2013}86400",
                ),
                (
                    "key=application.interval_report&value=abc",
                    "60\u{2013}86400",
                ),
                (
                    "key=application.interval_sample&value=4",
                    "0 or 5\u{2013}3600",
                ),
            ] {
                let html = body_text(
                    post(&router, &format!("/nodes/{NODE}/config"), &manager, form).await,
                )
                .await;
                assert!(html.contains(reason), "{form}: {html}");
            }
            assert!(
                state
                    .pending
                    .list(HOST, NODE)
                    .await
                    .expect("draft")
                    .is_empty(),
                "an invalid value never reaches the draft"
            );

            // 0 is the firmware's "sample on report", allowed beside the range.
            post(
                &router,
                &format!("/nodes/{NODE}/config"),
                &manager,
                "key=application.interval_sample&value=0",
            )
            .await;
            let rows = state.pending.list(HOST, NODE).await.expect("draft");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].desired_value.as_deref(), Some("0"));
        }

        /// The draft is shared state: a second session sees the first
        /// session's edit, and is *told* about it rather than having to ask.
        ///
        /// Both halves are asserted (§21 review, item 6). The doc used to
        /// claim "without a reload" while the test only did a second GET,
        /// which proves the draft is server-side and nothing about the push
        /// — so the nudge every other session's SSE connection listens on is
        /// asserted here too, at its source.
        #[tokio::test]
        async fn a_second_session_sees_the_same_banner() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let other = login(&state, "admin", "correct-horse-battery").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            // The other session's live connection, subscribed before the
            // edit — exactly what `sse::run` holds.
            let mut nudges = state.pending.subscribe_nudges();

            post(
                &router,
                &format!("/nodes/{NODE}/config"),
                &manager,
                "key=application.interval_report&value=600",
            )
            .await;

            let nudge = tokio::time::timeout(std::time::Duration::from_secs(2), nudges.recv())
                .await
                .expect("the edit is pushed, not only stored")
                .expect("nudge");
            assert_eq!(nudge.node_key, NODE, "and it names the Node to repaint");

            let html =
                body_text(get(&router, &format!("/nodes/{NODE}/pending"), &other).await).await;
            assert!(html.contains("application.interval_report"));
            assert!(
                html.contains("manager1"),
                "the banner names who changed it: {html}"
            );
        }

        /// A viewer can read the banner — a colleague's pending change is
        /// what explains the Node's next behaviour — and can do nothing to
        /// it. Both halves matter, so both are asserted.
        #[tokio::test]
        async fn a_viewer_cannot_draft_or_send() {
            let (state, _dir) = test_state().await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            for (path, body) in [
                (
                    format!("/nodes/{NODE}/config"),
                    "key=application.interval_report&value=600",
                ),
                (
                    format!("/nodes/{NODE}/thresholds/temperature"),
                    "critical_low=&warning_low=&warning_high=&critical_high=8",
                ),
                (format!("/nodes/{NODE}/pending/send"), ""),
                (format!("/nodes/{NODE}/pending/discard"), ""),
            ] {
                assert_eq!(
                    post(&router, &path, &viewer, body).await.status(),
                    StatusCode::FORBIDDEN,
                    "{path} must be manager-gated"
                );
            }
            assert!(state
                .pending
                .list(HOST, NODE)
                .await
                .expect("draft")
                .is_empty());

            // ...but the fragment is readable.
            assert_eq!(
                get(&router, &format!("/nodes/{NODE}/pending"), &viewer)
                    .await
                    .status(),
                StatusCode::OK
            );
        }

        /// Discard is a manager action on shared state, so any manager may
        /// throw away any manager's draft — and it leaves a trail.
        #[tokio::test]
        async fn discarding_drops_the_whole_draft_and_audits_it() {
            let (state, _dir) = test_state().await;
            let author = login_as_role(&state, Role::Manager, "manager1").await;
            let other = login_as_role(&state, Role::Manager, "manager2").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            post(
                &router,
                &format!("/nodes/{NODE}/config"),
                &author,
                "key=application.interval_report&value=600",
            )
            .await;
            assert_eq!(
                state.pending.list(HOST, NODE).await.expect("draft").len(),
                1
            );

            assert_eq!(
                post(
                    &router,
                    &format!("/nodes/{NODE}/pending/discard"),
                    &other,
                    ""
                )
                .await
                .status(),
                StatusCode::OK
            );
            assert!(state
                .pending
                .list(HOST, NODE)
                .await
                .expect("draft")
                .is_empty());

            let row = audit_rows(&state)
                .await
                .into_iter()
                .find(|r| r.event_type == "node_pending_discard")
                .expect("audited");
            assert_eq!(row.username.as_deref(), Some("manager2"));
            assert!(row.success);
        }

        /// §21 review, defect 1. A threshold edit is a DRAFT: the four
        /// bounds must not reach `field_thresholds` until a send walk
        /// drains, or the banner's "on the device" column quotes the
        /// operator's own unsent number back at them as fact, a Discard
        /// leaves it behind, and `plan_send` fills a field's untouched
        /// bounds from a table that already holds the draft.
        #[tokio::test]
        ///
        /// *Amended 2026-10-15 (config sync Q20d):* the Portal now stores and
        /// grades a critical bound at once; the draft only tracks delivery
        /// to the Node, and Discard stops that delivery without a revert.
        async fn a_critical_bound_is_stored_at_once_and_discard_keeps_it() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            let stored = || async {
                ({
                    let a0 = HOST.to_string();
                    let a1 = THRESHOLD_SCOPE_NODE.to_string();
                    let a2 = NODE.to_string();
                    state
                        .db
                        .run(move |s| s.list_field_thresholds(&a0, &a1, &a2))
                })
                .await
                .expect("read")
            };

            // The pre-edit truth: one row, written the only way a bound is
            // ever allowed to get here — a completed send.
            ({
                let a0 = NewFieldThreshold {
                    hostname: HOST.into(),
                    scope: THRESHOLD_SCOPE_NODE.into(),
                    entity: NODE.into(),
                    field: "temperature".into(),
                    critical_low: None,
                    warning_low: None,
                    warning_high: Some(6.0),
                    critical_high: Some(8.0),
                    enabled: true,
                };
                let a1 = Some("a previous send".to_string());
                state
                    .db
                    .write(move |s| s.upsert_field_threshold(&a0, a1.as_deref()))
            })
            .await
            .expect("seed the current bounds");

            // A manager drafts a new critical_high and clears `enabled`.
            assert_eq!(
                post(
                    &router,
                    &format!("/nodes/{NODE}/thresholds/temperature"),
                    &manager,
                    "critical_low=&warning_low=&warning_high=6&critical_high=9.5",
                )
                .await
                .status(),
                StatusCode::OK
            );
            assert!(
                state.relay.published().is_empty(),
                "a draft publishes nothing"
            );

            let rows = stored().await;
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].critical_high,
                Some(9.5),
                "Q20d: the critical bound is stored at once, before the Node confirms"
            );
            assert_eq!(rows[0].warning_high, Some(6.0));
            assert!(!rows[0].enabled, "Q20d: the switch is stored at once too");

            // Discard stops the delivery; the Portal keeps what was saved.
            assert_eq!(
                post(
                    &router,
                    &format!("/nodes/{NODE}/pending/discard"),
                    &manager,
                    ""
                )
                .await
                .status(),
                StatusCode::OK
            );
            let rows = stored().await;
            assert_eq!(rows[0].critical_high, Some(9.5));
            assert_eq!(rows[0].warning_high, Some(6.0));
            assert!(
                state
                    .pending
                    .list(HOST, NODE)
                    .await
                    .expect("draft")
                    .is_empty(),
                "the draft is what Discard throws away"
            );
        }

        /// A threshold edit on a field with no stored row must not invent bounds for it (§21
        /// review round 2, item 10). Since 2026-10-13 (devices.node.md amendment) a warning
        /// bound is stored at once and never drafted: a warning-only change leaves no draft at
        /// all, and the row it creates holds no critical bound nobody set.
        #[tokio::test]
        async fn a_warning_only_change_is_stored_at_once_and_drafts_nothing() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                &format!("/nodes/{NODE}/thresholds/temperature"),
                &manager,
                "critical_low=&warning_low=&warning_high=9.5&critical_high=&enabled=on",
            )
            .await;
            assert!(body_text(response)
                .await
                .contains(&tr("monitoring.threshold.warnings_saved")));

            let stored = state
                .db
                .run(|s| s.list_field_thresholds(HOST, THRESHOLD_SCOPE_NODE, NODE))
                .await
                .expect("read");
            assert_eq!(stored.len(), 1, "{stored:?}");
            let t = &stored[0];
            assert_eq!(t.field, "temperature");
            assert_eq!((t.warning_low, t.warning_high), (None, Some(9.5)));
            assert_eq!(
                (t.critical_low, t.critical_high),
                (None, None),
                "none invented"
            );
            assert!(t.enabled, "graded from now on");
            let draft = state.pending.list(HOST, NODE).await.expect("draft");
            assert!(draft.is_empty(), "nothing goes to the Node: {draft:?}");

            // The warning history records the write, like any other band write.
            let history = state
                .db
                .run(|s| s.threshold_history(HOST, THRESHOLD_SCOPE_NODE, NODE, Some("temperature")))
                .await
                .expect("history");
            assert_eq!(history.len(), 1);
        }

        /// Amendment 2026-10-13: a critical change drafts only the critical bounds; the
        /// warnings saved with it are stored at once, and the banner lists no warning row.
        #[tokio::test]
        async fn a_critical_change_drafts_only_the_critical_bounds() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            state
                .db
                .write(|s| {
                    s.upsert_field_threshold(
                        &portal_storage::NewFieldThreshold {
                            hostname: HOST.into(),
                            scope: THRESHOLD_SCOPE_NODE.into(),
                            entity: NODE.into(),
                            field: "temperature".into(),
                            critical_low: Some(0.0),
                            warning_low: Some(2.0),
                            warning_high: Some(6.0),
                            critical_high: Some(8.0),
                            enabled: true,
                        },
                        Some("seed"),
                    )
                })
                .await
                .expect("seed");
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                &format!("/nodes/{NODE}/thresholds/temperature"),
                &manager,
                "critical_low=0&warning_low=3&warning_high=7&critical_high=9&enabled=on",
            )
            .await;
            assert!(body_text(response)
                .await
                .contains(&tr("monitoring.pending.saved_sending")));
            let draft = state.pending.list(HOST, NODE).await.expect("draft");
            let keys: Vec<&str> = draft.iter().map(|r| r.param_key.as_str()).collect();
            assert_eq!(keys, ["threshold.temperature.critical_high"]);
            let stored = state
                .db
                .run(|s| s.get_field_threshold(HOST, THRESHOLD_SCOPE_NODE, NODE, "temperature"))
                .await
                .expect("read")
                .expect("row");
            assert_eq!(
                (stored.warning_low, stored.warning_high),
                (Some(3.0), Some(7.0))
            );
            assert_eq!(
                stored.critical_high,
                Some(9.0),
                "Q20d: the critical bound is stored at once"
            );

            // A warnings-only edit afterwards keeps the critical draft and drafts nothing new.
            post(
                &router,
                &format!("/nodes/{NODE}/thresholds/temperature"),
                &manager,
                "critical_low=0&warning_low=1&warning_high=7&critical_high=9&enabled=on",
            )
            .await;
            let draft = state.pending.list(HOST, NODE).await.expect("draft");
            let keys: Vec<&str> = draft.iter().map(|r| r.param_key.as_str()).collect();
            assert_eq!(keys, ["threshold.temperature.critical_high"]);

            // The banner shows the critical row only.
            let html =
                body_text(get(&router, &format!("/nodes/{NODE}/pending"), &manager).await).await;
            assert!(html.contains("critical_high") || html.contains("Kritick"));
            assert!(!html.contains("threshold.temperature.warning"));
        }

        /// A draft from before 2026-10-13 still holds warning rows. A save of that field drops
        /// them: the warnings it posts are stored instead.
        #[tokio::test]
        async fn a_legacy_warning_draft_row_is_dropped_by_the_next_save() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            state
                .pending
                .upsert(portal_storage::NewPendingConfig {
                    hostname: HOST.into(),
                    node_key: NODE.into(),
                    param_key: crate::pending::threshold_param_key("temperature", "warning_high"),
                    kind: portal_storage::PENDING_KIND_THRESHOLD.into(),
                    desired_value: Some("12.0".into()),
                    author: "old".into(),
                })
                .await
                .expect("legacy row");
            let router = super::super::router().with_state(state.clone());
            post(
                &router,
                &format!("/nodes/{NODE}/thresholds/temperature"),
                &manager,
                "critical_low=&warning_low=&warning_high=11&critical_high=&enabled=on",
            )
            .await;
            let draft = state.pending.list(HOST, NODE).await.expect("draft");
            assert!(draft.is_empty(), "{draft:?}");
            let stored = state
                .db
                .run(|s| s.get_field_threshold(HOST, THRESHOLD_SCOPE_NODE, NODE, "temperature"))
                .await
                .expect("read")
                .expect("row");
            assert_eq!(stored.warning_high, Some(11.0));
        }

        /// UX-038: a pressure band is typed in hPa and drafted in the hPa the
        /// v2 decoder takes, unscaled; the chart endpoint answers in hPa.
        #[tokio::test]
        async fn a_pressure_band_is_typed_and_drafted_in_hpa() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            // A Node takes bounds only for what it measures (§3.4): this one has reported
            // pressure, in hPa.
            state.store.apply(IngestEvent::NodeReading {
                hostname: HOST.into(),
                node_key: NODE.into(),
                payload: Box::new(crate::state::NodeReadingMsg {
                    message_id: "m-pressure".into(),
                    dev_eui: NODE.into(),
                    serial: None,
                    fields: [("pressure".to_string(), 1013.2)].into_iter().collect(),
                    counters: Default::default(),
                    events: Vec::new(),
                    gateways: Vec::new(),
                    rssi: None,
                    snr: None,
                    fport: None,
                    page_index: None,
                    page_count: None,
                    dr: None,
                    received_at: "2026-09-28T08:00:00Z".to_string(),
                }),
            });
            let router = super::super::router().with_state(state.clone());

            post(
                &router,
                &format!("/nodes/{NODE}/thresholds/pressure"),
                &manager,
                "critical_low=990&warning_low=1000&warning_high=1025.5&critical_high=&enabled=on",
            )
            .await;
            let draft = state.pending.list(HOST, NODE).await.expect("draft");
            let bound = |b: &str| {
                draft
                    .iter()
                    .find(|r| r.param_key == crate::pending::threshold_param_key("pressure", b))
                    .and_then(|r| r.desired_value.clone())
            };
            assert_eq!(bound("critical_low").as_deref(), Some("990.0"));
            // The warnings are stored at once, in hPa too (amendment 2026-10-13).
            assert_eq!(bound("warning_low"), None);
            let stored = state
                .db
                .run(|s| s.get_field_threshold(HOST, THRESHOLD_SCOPE_NODE, NODE, "pressure"))
                .await
                .expect("read")
                .expect("row");
            assert_eq!(
                (stored.warning_low, stored.warning_high),
                (Some(1000.0), Some(1025.5))
            );
            // P-3: the blank critical bound is completed with the open end of the band.
            assert_eq!(bound("critical_high").as_deref(), Some("9999.0"));

            let json: serde_json::Value = serde_json::from_str(
                &body_text(
                    get(
                        &router,
                        &format!("/api/v1/chart/node/{NODE}/pressure"),
                        &manager,
                    )
                    .await,
                )
                .await,
            )
            .expect("chart json");
            assert_eq!(json["unit"], "hPa");
        }

        /// threshold_defaults.md D5 (amending OQ3 for a band that is off): a band saved
        /// switched off on a field with no band, or one that is off, is the portal's alone —
        /// stored off, nothing drafted, nothing sent — like the default band itself. Switching
        /// it on drafts as always, and a save while that draft waits drafts too.
        #[tokio::test]
        async fn a_band_saved_off_is_stored_off_and_drafts_nothing() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());
            let rows = |state: &AppState| {
                let state = state.clone();
                async move {
                    state
                        .db
                        .run(|s| s.list_field_thresholds(HOST, THRESHOLD_SCOPE_NODE, NODE))
                        .await
                        .expect("read")
                }
            };

            // `enabled` absent — the checkbox is off.
            let html = body_text(
                post(
                    &router,
                    &format!("/nodes/{NODE}/thresholds/temperature"),
                    &manager,
                    "critical_low=-10&warning_low=&warning_high=9.5&critical_high=50",
                )
                .await,
            )
            .await;
            assert!(
                html.contains(&tr("monitoring.threshold.saved_off")),
                "{html}"
            );
            let stored = rows(&state).await;
            assert_eq!(stored.len(), 1);
            assert!(!stored[0].enabled);
            assert_eq!(stored[0].warning_high, Some(9.5));
            assert!(state
                .pending
                .list(HOST, NODE)
                .await
                .expect("draft")
                .is_empty());
            assert!(state.relay.published().is_empty(), "nothing is sent");

            // Switched on: the switch is drafted, the bounds the portal holds are not.
            post(
                &router,
                &format!("/nodes/{NODE}/thresholds/temperature"),
                &manager,
                "critical_low=-10&warning_low=&warning_high=9.5&critical_high=50&enabled=on",
            )
            .await;
            let draft = state.pending.list(HOST, NODE).await.expect("draft");
            let keys: Vec<&str> = draft.iter().map(|r| r.param_key.as_str()).collect();
            assert_eq!(keys, ["threshold.temperature.enabled"]);
            assert!(rows(&state).await[0].enabled, "Q20d: on at once");

            // Off again while that draft waits: the draft path, which drops the switch it
            // no longer needs and stores nothing.
            //
            // *Amended 2026-10-15 (Q20b):* the "on" may already wait in the
            // Hub's queue, so the switch stays drafted as "off" and its send
            // supersedes the waiting one.
            post(
                &router,
                &format!("/nodes/{NODE}/thresholds/temperature"),
                &manager,
                "critical_low=-10&warning_low=&warning_high=9.5&critical_high=50",
            )
            .await;
            let draft = state.pending.list(HOST, NODE).await.expect("draft");
            let drafted: Vec<(&str, Option<&str>)> = draft
                .iter()
                .map(|r| (r.param_key.as_str(), r.desired_value.as_deref()))
                .collect();
            assert_eq!(drafted, [("threshold.temperature.enabled", Some("false"))]);
            assert!(!rows(&state).await[0].enabled);
            assert_eq!(rows(&state).await[0].warning_high, Some(9.5));
        }

        /// §21 review, item 4. Two managers pressing Send at the same moment
        /// used to start two walks over one draft — both marking the same
        /// rows, both waiting on confirmations that carry no request id.
        #[tokio::test]
        async fn two_concurrent_sends_start_one_walk() {
            let (state, _dir) = test_state().await;
            let first = login_as_role(&state, Role::Manager, "manager1").await;
            let second = login_as_role(&state, Role::Manager, "manager2").await;
            seed_node(&state, NODE);
            // The walk mints a signing key before it publishes, so the Hub
            // has to be paired or the first Send never gets far enough to
            // hold the claim.
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            post(
                &router,
                &format!("/nodes/{NODE}/config"),
                &first,
                "key=application.interval_report&value=600",
            )
            .await;

            // The first Send claims the Node and spawns a walk that is now
            // waiting on the relay.
            let started =
                body_text(post(&router, &format!("/nodes/{NODE}/pending/send"), &first, "").await)
                    .await;
            assert!(
                started.contains(&tr("monitoring.pending.send_started")),
                "the first Send answers immediately: {started}"
            );

            // The second is refused while that walk is in flight.
            let refused =
                body_text(post(&router, &format!("/nodes/{NODE}/pending/send"), &second, "").await)
                    .await;
            // Matched on the distinctive tail: minijinja HTML-escapes the
            // apostrophe in the full string.
            assert!(
                refused.contains("already being sent"),
                "a second Send is refused, not run: {refused}"
            );

            // The refusal is not a send, and writes no send audit row.
            //
            // Exactly zero, not "at most one": the walk that IS running
            // audits when it *finishes*, and it is still parked waiting for
            // the relay at this point. `<= 1` was vacuous — it held whether
            // or not the second Send had run.
            let sends = audit_rows(&state)
                .await
                .into_iter()
                .filter(|r| r.event_type == "node_pending_send")
                .count();
            assert_eq!(sends, 0, "the refused Send wrote no send audit row");
        }

        /// A send with nothing to send is an error the operator can read,
        /// not a silent success and not a published command.
        #[tokio::test]
        async fn sending_an_empty_draft_publishes_nothing() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                &format!("/nodes/{NODE}/pending/send"),
                &manager,
                "",
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert!(state.relay.published().is_empty());
        }

        // ---- WP4.3: the service card -----------------------------------

        /// devices.node.md §3.6: service functions are admin. A manager —
        /// who may register, remove and reconfigure Nodes all day — cannot
        /// reboot one.
        #[tokio::test]
        async fn every_service_route_is_admin_only() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            for (path, body) in [
                (format!("/nodes/{NODE}/service/force-send"), ""),
                (format!("/nodes/{NODE}/service/send-msg"), "payload=%7B%7D"),
                (
                    format!("/nodes/{NODE}/service/reboot"),
                    &format!("confirm={NODE}"),
                ),
            ] {
                assert_eq!(
                    post(&router, &path, &manager, body).await.status(),
                    StatusCode::FORBIDDEN,
                    "{path} must be admin-only"
                );
            }
            assert_eq!(
                get(
                    &router,
                    &format!("/nodes/{NODE}/service/force-send/watch?since=0&until=1"),
                    &manager
                )
                .await
                .status(),
                StatusCode::FORBIDDEN
            );
            assert!(state.relay.published().is_empty());
        }

        /// The service functions go through the in-process control.radio seam, never the signed
        /// relay: on a v2 Hub the relay's `fiber_app` has no LoRaWAN stack and sent nothing
        /// (bench, 2026-09-23). The test seam has no radio config, so its refusal is what
        /// renders — proof the seam, not the relay, answered. On success `Cmd::radio` audits
        /// (§11 gap H: asking a battery device to describe itself costs it airtime).
        #[tokio::test]
        async fn service_functions_go_through_the_radio_seam_never_the_relay() {
            let (state, _dir) = test_state().await;
            let admin = login(&state, "admin", "correct-horse-battery").await;
            seed_node(&state, NODE);
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            for (path, body) in [
                (format!("/nodes/{NODE}/read-device"), ""),
                (format!("/nodes/{NODE}/service/force-send"), ""),
                (
                    format!("/nodes/{NODE}/service/send-msg"),
                    "payload=%7B%22application.history_enable%22%3A+true%7D",
                ),
                (
                    format!("/nodes/{NODE}/service/reboot"),
                    &format!("confirm={NODE}") as &str,
                ),
            ] {
                let text = body_text(post(&router, &path, &admin, body).await).await;
                assert!(
                    text.contains("no rendered radio config"),
                    "{path} answered by the seam: {text}"
                );
            }
            assert!(
                state.relay.published().is_empty(),
                "nothing goes over the legacy relay"
            );
        }

        #[test]
        fn a_service_message_becomes_set_config_assignments() {
            let config = serde_json::json!({
                "firmware.future_key": 1,
                "application.history_enable": true,
                "alarms.alarm_0": "0300",
            });
            let mut got = crate::radio::config_assignments(config.as_object().expect("object"));
            got.sort();
            assert_eq!(
                got,
                vec![
                    "alarms.alarm_0=0300".to_string(),
                    "application.history_enable=true".to_string(),
                    "firmware.future_key=1".to_string(),
                ]
            );
        }

        /// nodes_phase1.cs.md WP6: the raw message refuses the keys the portal owns or that
        /// are not the radio's before anything is sent; the rest goes to the seam, whose key
        /// table and the firmware registry check it.
        #[tokio::test]
        async fn a_raw_message_refuses_the_portals_keys_and_sends_the_rest() {
            let (state, _dir) = test_state().await;
            let admin = login(&state, "admin", "correct-horse-battery").await;
            seed_node(&state, NODE);
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            for payload in [
                r#"{"alarms.alarm_0": "03"}"#,
                r#"{"application.history_sensors": 3}"#,
                r#"{"application.history_channels": "0001"}"#,
                r#"{"sensors.sensor1_rom": "28ff"}"#,
                r#"{"lorawan.adr": true}"#,
                r#"{"application.vendor_reset_allow": false, "application.calibration": true}"#,
            ] {
                let encoded: String = payload
                    .bytes()
                    .map(|b| match b {
                        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-' => {
                            char::from(b).to_string()
                        }
                        _ => format!("%{b:02X}"),
                    })
                    .collect();
                let body = format!("payload={encoded}");
                let text = body_text(
                    post(
                        &router,
                        &format!("/nodes/{NODE}/service/send-msg"),
                        &admin,
                        &body,
                    )
                    .await,
                )
                .await;
                assert!(
                    text.contains("Not sent through a raw message"),
                    "{payload}: {text}"
                );
                assert!(
                    !text.contains("no rendered radio config"),
                    "refused before the seam"
                );
            }
            let text = body_text(
                post(
                    &router,
                    &format!("/nodes/{NODE}/service/send-msg"),
                    &admin,
                    "payload=%7B%22application.calibration%22%3A+true%7D",
                )
                .await,
            )
            .await;
            assert!(
                text.contains("no rendered radio config"),
                "an allowed key reaches the seam: {text}"
            );
            assert!(state.relay.published().is_empty(), "never the legacy relay");
        }

        /// PN-3 and P2: a P2P Node's reading arrives up to two intervals and a couple of
        /// seconds after the send, so the watch must outlast that; and it waits exactly as
        /// long as a config send waits for its ack, known interval or not.
        #[test]
        fn the_force_send_wait_is_the_answer_budget() {
            let report = 60;
            let worst_p2p = 2 * report + 2;
            assert!(super::super::force_send_wait_secs(Some(report)) > worst_p2p);
            assert_eq!(super::super::force_send_wait_secs(Some(900)), 1860);
            for interval in [None, Some(30), Some(600), Some(900)] {
                assert_eq!(
                    super::super::force_send_wait_secs(interval),
                    crate::pending::downlink_answer_budget(
                        interval.and_then(|r: i64| u64::try_from(r).ok())
                    )
                    .as_secs() as i64,
                    "{interval:?}"
                );
            }
        }

        /// Force send has no fPort-85 answer: the first reading at or after the send
        /// confirms it, and past the deadline without one it had no response.
        #[tokio::test]
        async fn the_force_send_watch_is_confirmed_by_the_next_reading() {
            let (state, dir) = test_state().await;
            let admin = login(&state, "admin", "correct-horse-battery").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());
            let hostname = super::super::hostname_for_known_node(&state, NODE).expect("known node");
            let watch = |since: i64, until: i64| {
                format!("/nodes/{NODE}/service/force-send/watch?since={since}&until={until}")
            };
            let far = super::super::now_unix() + 3600;

            let waiting = body_text(get(&router, &watch(1_000, far), &admin).await).await;
            assert!(waiting.contains("waiting for the Node"), "{waiting}");
            assert!(waiting.contains("every 15s"), "keeps polling");

            let late = body_text(get(&router, &watch(1_000, 2_000), &admin).await).await;
            assert!(late.contains("No response"), "{late}");
            assert!(!late.contains("every 15s"), "stops polling");

            {
                let storage = portal_storage::Storage::open(&dir.path().join("portal.db"), None)
                    .expect("open storage");
                // A command answer (fPort 85) and an alarm (fPort 3) confirm nothing.
                storage
                    .insert_raw_message(&hostname, NODE, 1_500, r#"{"fport":85}"#)
                    .expect("insert");
                storage
                    .insert_raw_message(&hostname, NODE, 1_600, r#"{"fport":3}"#)
                    .expect("insert");
            }
            let still = body_text(get(&router, &watch(1_000, far), &admin).await).await;
            assert!(still.contains("waiting for the Node"), "{still}");
            {
                let storage = portal_storage::Storage::open(&dir.path().join("portal.db"), None)
                    .expect("open storage");
                storage
                    .insert_raw_message(&hostname, NODE, 900, r#"{"fport":2}"#)
                    .expect("insert");
                storage
                    .insert_raw_message(&hostname, NODE, 1_700, r#"{"fport":2}"#)
                    .expect("insert");
            }
            // The uplink that carried the command down is no answer yet.
            let carried = body_text(get(&router, &watch(1_000, far), &admin).await).await;
            assert!(carried.contains("waiting for the Node"), "{carried}");
            {
                let storage = portal_storage::Storage::open(&dir.path().join("portal.db"), None)
                    .expect("open storage");
                storage
                    .insert_raw_message(&hostname, NODE, 1_702, r#"{"fport":2}"#)
                    .expect("insert");
            }
            let confirmed = body_text(get(&router, &watch(1_000, far), &admin).await).await;
            assert!(confirmed.contains("Confirmed"), "{confirmed}");
            assert!(!confirmed.contains("every 15s"));
        }

        /// The typed confirmation is checked on the server. A mismatched
        /// one publishes nothing and is itself audited — it is either a typo
        /// or somebody trying the URL.
        #[tokio::test]
        async fn reboot_refuses_a_mismatched_confirmation_and_publishes_nothing() {
            let (state, _dir) = test_state().await;
            let admin = login(&state, "admin", "correct-horse-battery").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            let html = body_text(
                post(
                    &router,
                    &format!("/nodes/{NODE}/service/reboot"),
                    &admin,
                    "confirm=yes-please",
                )
                .await,
            )
            .await;
            assert!(html.contains("not this Node"), "{html}");
            assert!(state.relay.published().is_empty());

            let row = audit_rows(&state)
                .await
                .into_iter()
                .find(|r| r.event_type == "node_service_reboot")
                .expect("a refused reboot is audited too");
            assert!(!row.success);
        }

        // ---- WP4.4: lifecycle -------------------------------------------

        /// Every check the device agent makes runs here first, so a typo is
        /// a red field rather than a signed-command failure minutes later.
        #[tokio::test]
        async fn registering_a_node_validates_before_it_publishes() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            for (body, expected) in [
                (
                    "serial=1&dev_eui=toolong0000000000&app_key=00000000000000000000000000000000",
                    "invalid_dev_eui",
                ),
                (
                    "serial=1&dev_eui=70b3d5e05001aaaa&app_key=short",
                    "invalid_app_key",
                ),
                (
                    "serial=abc&dev_eui=70b3d5e05001aaaa&app_key=00000000000000000000000000000000",
                    "invalid_serial",
                ),
                (
                    "serial=1&dev_eui=70b3d5e05001aaaa&app_key=00000000000000000000000000000000&join_eui=5876",
                    "invalid_join_eui",
                ),
            ] {
                let response = post(&router, "/nodes", &manager, body).await;
                assert_eq!(response.status(), StatusCode::SEE_OTHER);
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                assert_eq!(location, format!("/nodes?error={expected}"), "for {body}");
            }
            assert!(
                state.relay.published().is_empty(),
                "nothing invalid reached the wire"
            );
        }

        /// A Hub `h` that says it is a cluster Client (`system/info.setup.role`), with its own
        /// gateway EUI and JoinEUI — what a Client with a concentrator reports.
        fn seed_client(state: &AppState, h: &str, gateway_eui: &str, join_eui: &str) {
            use fiber_protocol::payloads::system_info::{
                LorawanGatewayInfo, SetupInfo, SystemInfoMsg,
            };
            state.store.apply(IngestEvent::Status {
                hostname: h.into(),
                status: DeviceStatus::Online,
            });
            state.store.apply(IngestEvent::SystemInfo {
                hostname: h.into(),
                payload: Box::new(SystemInfoMsg {
                    setup: Some(SetupInfo {
                        provisioned: Some(true),
                        role: Some("client".into()),
                    }),
                    lorawan: Some(LorawanGatewayInfo {
                        mode: Some("lorawan".into()),
                        gateway_eui: Some(gateway_eui.into()),
                        join_eui: Some(join_eui.into()),
                        ..LorawanGatewayInfo::default()
                    }),
                    ..SystemInfoMsg::default()
                }),
            });
        }

        /// The Server `HOST` reports its own JoinEUI, as `system/info.lorawan.join_eui`.
        fn seed_server_join_eui(state: &AppState, join_eui: &str) {
            use fiber_protocol::payloads::system_info::{LorawanGatewayInfo, SystemInfoMsg};
            state.store.apply(IngestEvent::SystemInfo {
                hostname: HOST.into(),
                payload: Box::new(SystemInfoMsg {
                    lorawan: Some(LorawanGatewayInfo {
                        join_eui: Some(join_eui.into()),
                        ..LorawanGatewayInfo::default()
                    }),
                    ..SystemInfoMsg::default()
                }),
            });
        }

        /// devices.node.md §5, dodatek 2026-10-13: the dialog offers no Hub, even with two
        /// in the store, prefills the Server's JoinEUI, and a registration lands on the Server.
        #[tokio::test(start_paused = true)]
        async fn registering_in_a_cluster_names_no_hub_and_lands_on_the_server() {
            let (mut state, _dir) = test_state().await;
            state.local_hostname = HOST.to_string();
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            seed_server_join_eui(&state, "5876070000000000");
            // Alphabetically first, so the old dialog preselected it.
            seed_client(&state, "a-client", "70b3d54900aa0002", "1111111111111111");
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            let page = body_text(get(&router, "/nodes/add", &manager).await).await;
            assert!(page.contains(r#"action="/nodes""#), "the form renders");
            assert!(
                !page.contains(r#"name="hub""#),
                "the dialog offers no Hub: {page}"
            );
            assert!(
                page.contains(r#"value="5876070000000000""#),
                "the Server's JoinEUI"
            );
            assert!(
                !page.contains("1111111111111111"),
                "never the Client's JoinEUI"
            );

            let body = "serial=7&dev_eui=70b3d5e05001bbbb&app_key=00000000000000000000000000000000";
            post(&router, "/nodes", &manager, body).await;
            let published = state.relay.published();
            let (topic, _) = published.first().expect("published to the Server");
            assert!(topic.contains(HOST), "{topic}");
            assert!(!topic.contains("a-client"), "{topic}");
        }

        /// A POST naming a Client — the form field or `?hub=` — is refused with the Czech
        /// i18n sentence naming the Server, before anything is signed. Naming the Server
        /// itself is fine.
        #[tokio::test(start_paused = true)]
        async fn a_registration_naming_a_client_is_refused() {
            let (mut state, _dir) = test_state().await;
            state.local_hostname = HOST.to_string();
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            seed_client(&state, "h2", "70b3d54900aa0002", "1111111111111111");
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());
            let body = "serial=7&dev_eui=70b3d5e05001bbbb&app_key=00000000000000000000000000000000";

            let expected = tr("monitoring.nodes.err_hub_not_server")
                .replace("{hub}", "h2")
                .replace("{server}", HOST);
            for (uri, form) in [
                ("/nodes".to_string(), format!("{body}&hub=h2")),
                ("/nodes?hub=h2".to_string(), body.to_string()),
            ] {
                let page = body_text(post(&router, &uri, &manager, &form).await).await;
                assert!(page.contains(&expected), "{uri} {form}: {page}");
                assert!(
                    state.relay.published().is_empty(),
                    "nothing went to a Client"
                );
            }
            // The Czech sentence exists and names both.
            let cs: serde_json::Value =
                serde_json::from_str(include_str!("../../i18n/cs/monitoring.json")).unwrap();
            let sentence = cs["monitoring.nodes.err_hub_not_server"].as_str().unwrap();
            assert!(
                sentence.contains("{server}") && sentence.contains("{hub}"),
                "{sentence}"
            );

            post(&router, "/nodes", &manager, &format!("{body}&hub={HOST}")).await;
            let published = state.relay.published();
            let (topic, _) = published.first().expect("the Server takes it");
            assert!(topic.contains(HOST), "{topic}");
        }

        /// No local Hub in the store and two Hubs that could be Servers: the portal does not
        /// guess. One Hub that is not a Client is the Server even without `local_hostname`.
        #[tokio::test]
        async fn without_a_known_server_registration_is_refused() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            state.store.apply(IngestEvent::Status {
                hostname: "h2".into(),
                status: DeviceStatus::Online,
            });
            let router = super::super::router().with_state(state.clone());
            let body = "serial=7&dev_eui=70b3d5e05001bbbb&app_key=00000000000000000000000000000000";
            let response = post(&router, "/nodes", &manager, body).await;
            assert_eq!(
                response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/nodes?error=no_server")
            );
            assert!(state.relay.published().is_empty());

            seed_client(&state, "h2", "70b3d54900aa0002", "1111111111111111");
            assert_eq!(
                super::super::registration_server(&state).as_deref(),
                Some(HOST)
            );
        }

        /// The radio choice is the Server's `radio.mode`, whatever a Client runs: a P2P Server
        /// with a LoRaWAN Client offers LoRaWAN disabled and refuses it on POST. And the
        /// Register button follows the Server's pairing alone, not the Client's.
        #[tokio::test(start_paused = true)]
        async fn radio_options_and_the_paired_gate_follow_the_server() {
            use control_radio::Mode;
            let (mut state, _dir) = test_state().await;
            let radio_dir = tempfile::tempdir().expect("tempdir");
            state.radio = crate::radio::RadioControl::for_tests(Mode::P2p, radio_dir.path());
            state.local_hostname = HOST.to_string();
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            seed_client(&state, "h2", "70b3d54900aa0002", "1111111111111111");
            let router = super::super::router().with_state(state.clone());

            // Neither Hub paired: the Server's pairing blocks registration.
            let page = body_text(get(&router, "/nodes/add", &manager).await).await;
            let submit = page
                .split(r#"type="submit" class="btn-v1 btn-v1-primary""#)
                .nth(1)
                .expect("the dialog's submit")
                .split('>')
                .next()
                .unwrap_or_default()
                .to_string();
            assert!(submit.contains("disabled"), "unpaired Server: {submit}");

            // The Client paired, the Server not: still blocked.
            state
                .keys
                .store_pairing(
                    "h2",
                    Some("test-ca".into()),
                    "00".repeat(32),
                    [7u8; 32],
                    None,
                    None,
                )
                .await
                .expect("store pairing");
            let page = body_text(get(&router, "/nodes/add", &manager).await).await;
            let submit = page
                .split(r#"type="submit" class="btn-v1 btn-v1-primary""#)
                .nth(1)
                .unwrap_or_default()
                .split('>')
                .next()
                .unwrap_or_default()
                .to_string();
            assert!(
                submit.contains("disabled"),
                "a paired Client does not count: {submit}"
            );

            pair_hub(&state).await;
            let page = body_text(get(&router, "/nodes/add", &manager).await).await;
            let lorawan_input = page
                .split(r#"name="radio" value="lorawan""#)
                .nth(1)
                .expect("the LoRaWAN option")
                .split('>')
                .next()
                .unwrap_or_default()
                .to_string();
            assert!(lorawan_input.contains("disabled"), "{lorawan_input}");
            let p2p_input = page
                .split(r#"name="radio" value="p2p""#)
                .nth(1)
                .expect("the P2P option")
                .split('>')
                .next()
                .unwrap_or_default()
                .to_string();
            assert!(!p2p_input.contains("disabled"), "{p2p_input}");
            assert!(p2p_input.contains("checked"), "{p2p_input}");

            let response = post(
                &router,
                "/nodes",
                &manager,
                "radio=lorawan&serial=7&dev_eui=70b3d5e05001bbbb&app_key=00000000000000000000000000000000",
            )
            .await;
            assert_eq!(
                response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/nodes?error=radio_off_lorawan")
            );
            assert!(state.relay.published().is_empty());
        }

        /// O-3: a POST without a required field gets the dialog's flash, not a bare 422.
        #[tokio::test]
        async fn a_registration_missing_a_field_gets_the_dialogs_flash() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());
            for (body, flash) in [
                ("serial=1", "/nodes?error=invalid_dev_eui"),
                (
                    "serial=1&dev_eui=70b3d5e05001bbbb",
                    "/nodes?error=invalid_app_key",
                ),
                (
                    "dev_eui=70b3d5e05001bbbb&app_key=00000000000000000000000000000000",
                    "/nodes?error=invalid_serial",
                ),
            ] {
                let response = post(&router, "/nodes", &manager, body).await;
                assert_eq!(
                    response
                        .headers()
                        .get(header::LOCATION)
                        .and_then(|v| v.to_str().ok()),
                    Some(flash),
                    "{body}"
                );
            }
            assert!(state.relay.published().is_empty());
        }

        /// A DevEUI the Hub already has is a conflict. Reporting success
        /// would leave the operator believing they registered a device they
        /// did not.
        #[tokio::test]
        async fn a_duplicate_registration_is_refused_not_faked() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                "/nodes",
                &manager,
                &format!("serial=1&dev_eui={NODE}&app_key=00000000000000000000000000000000"),
            )
            .await;
            assert_eq!(
                response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/nodes?error=already_exists")
            );
            assert!(state.relay.published().is_empty());
        }

        /// The AppKey goes into the signed params and nowhere else. This
        /// test greps every audit row and every rendered byte for it.
        #[tokio::test(start_paused = true)]
        async fn the_app_key_never_reaches_the_audit_row_or_the_flash() {
            const APP_KEY: &str = "0123456789abcdef0123456789abcdef";
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                "/nodes",
                &manager,
                &format!("serial=42&dev_eui=70b3d5e05001aaaa&app_key={APP_KEY}"),
            )
            .await;
            let headers = format!("{:?}", response.headers());
            let rendered = format!("{headers}{}", body_text(response).await);
            assert!(
                !rendered.contains(APP_KEY),
                "the AppKey must not reach the response: {rendered}"
            );

            for row in audit_rows(&state).await {
                let serialised = format!("{row:?}");
                assert!(
                    !serialised.contains(APP_KEY),
                    "the AppKey must not reach an audit row: {serialised}"
                );
            }

            // It DID go to the wire, which is the whole point of typing it.
            // `node-add` is signed, so the params ride inside the
            // config/request envelope, in control.radio.md §7's shape.
            let published = state.relay.published();
            let (topic, body) = published.first().expect("published");
            assert!(topic.ends_with("commands/config/request"), "{topic}");
            assert_eq!(body["command_type"], "node-add");
            let payload = &body["params"];
            assert_eq!(payload["app_key"], APP_KEY);
            // `name` falls back to the serial when the field is blank.
            assert_eq!(payload["name"], "42");
            assert_eq!(payload["serial"], "42");
            assert_eq!(payload["radio"], "lorawan");
            assert_eq!(payload["profile"], "default");
            assert_eq!(payload["dev_eui"], "70b3d5e05001aaaa");
            assert!(payload.get("mode").is_none(), "no legacy field: {payload}");
            // #86: the field was not in this form, so nothing overrides the Hub default.
            assert!(payload.get("join_eui").is_none(), "{payload}");
        }

        /// threshold_defaults.md §3: a Node the portal registers gets its board's default
        /// bands at once, switched off, and nothing else — no draft, no command but the
        /// registration itself. A refused registration writes none.
        #[tokio::test]
        async fn a_registered_node_gets_switched_off_default_bands() {
            const APP_KEY: &str = "0123456789abcdef0123456789abcdef";
            const DEV: &str = "70b3d5e05001aaaa";
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            let r = router.clone();
            let token = manager.clone();
            let response = with_signed_success(&state, move || async move {
                post(
                    &r,
                    "/nodes",
                    &token,
                    &format!("serial=42&dev_eui={DEV}&app_key={APP_KEY}"),
                )
                .await
            })
            .await;
            assert_eq!(
                response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/nodes?ok=registered")
            );
            let rows = state
                .db
                .run(|s| s.list_field_thresholds(HOST, THRESHOLD_SCOPE_NODE, DEV))
                .await
                .expect("rows");
            let fields: Vec<&str> = rows.iter().map(|r| r.field.as_str()).collect();
            assert_eq!(fields, ["humidity", "temperature"]);
            assert!(rows.iter().all(|r| !r.enabled), "{rows:?}");
            assert!(state
                .pending
                .list(HOST, DEV)
                .await
                .expect("draft")
                .is_empty());
            let published: Vec<String> = state
                .relay
                .published()
                .into_iter()
                .filter(|(topic, _)| topic.ends_with("commands/config/request"))
                .map(|(_, body)| body["command_type"].as_str().unwrap_or("").to_string())
                .collect();
            assert_eq!(published, ["node-add"]);
        }

        /// PN-8 and PU-1: a registration goes out on the carrier the one resolver names (the
        /// Hub's `radio.mode`, P2P on a `dual` Hub), and its audit row says that carrier.
        /// The JoinEUI rides a LoRaWAN registration only. One scenario per carrier.
        #[tokio::test(start_paused = true)]
        async fn a_registration_goes_out_and_is_audited_on_the_resolved_carrier() {
            use control_radio::Mode;
            // A test value, as in the AppKey test above.
            const APP_KEY: &str = "0123456789abcdef0123456789abcdef";
            for (mode, carrier) in [
                (Mode::Lorawan, "lorawan"),
                (Mode::P2p, "p2p"),
                (Mode::Dual, "p2p"),
            ] {
                let (mut state, _dir) = test_state().await;
                let radio_dir = tempfile::tempdir().expect("tempdir");
                state.radio = crate::radio::RadioControl::for_tests(mode, radio_dir.path());
                let manager = login_as_role(&state, Role::Manager, "manager1").await;
                seed_node(&state, NODE);
                pair_hub(&state).await;
                let router = super::super::router().with_state(state.clone());

                post(
                    &router,
                    "/nodes",
                    &manager,
                    &format!(
                        "serial=42&dev_eui=70b3d5e05001aaaa&app_key={APP_KEY}\
                         &join_eui=70b3d57ed0000001"
                    ),
                )
                .await;
                let published = state.relay.published();
                let (_, body) = published.first().expect("published");
                assert_eq!(body["command_type"], "node-add", "{mode:?}");
                assert_eq!(body["params"]["radio"], carrier, "{mode:?}");
                assert_eq!(
                    body["params"].get("join_eui").is_some(),
                    carrier == "lorawan",
                    "{mode:?}: {}",
                    body["params"]
                );
                let details = audit_details(&state, "node_add").await;
                assert_eq!(details["transport"], carrier, "{mode:?}: {details}");
            }
        }

        /// Decision #23 (amended 2026-09-27): a Node is registered on one carrier. A DevEUI a
        /// registry of the Hub holds is refused before anything is signed, and the flash names
        /// the carrier to remove it from. Both carriers.
        #[tokio::test]
        async fn a_dev_eui_already_on_a_carrier_is_refused_naming_it() {
            use control_radio::Mode;
            const APP_KEY: &str = "0123456789abcdef0123456789abcdef";
            const DEV: &str = "70b3d5e05001aaaa";
            for carrier in portal_domain::Transport::ALL {
                let (mut state, _dir) = test_state().await;
                let radio_dir = tempfile::tempdir().expect("tempdir");
                state.radio = crate::radio::RadioControl::for_tests(Mode::Dual, radio_dir.path());
                crate::radio::test_support::register(radio_dir.path(), carrier, DEV);
                let manager = login_as_role(&state, Role::Manager, "manager1").await;
                pair_hub(&state).await;
                let router = super::super::router().with_state(state.clone());

                let response = post(
                    &router,
                    "/nodes",
                    &manager,
                    &format!("serial=42&dev_eui={DEV}&app_key={APP_KEY}"),
                )
                .await;
                let code = format!("registered_on_{}", carrier.as_str());
                assert_eq!(
                    response
                        .headers()
                        .get(header::LOCATION)
                        .and_then(|v| v.to_str().ok()),
                    Some(&*format!("/nodes?error={code}")),
                    "{carrier:?}"
                );
                assert!(state.relay.published().is_empty(), "{carrier:?}");
                let flash = super::super::node_list_flash(&code);
                assert!(
                    flash.contains(&tr(&format!("monitoring.transport.{}", carrier.as_str()))),
                    "{carrier:?}: {flash}"
                );
            }
        }

        /// #470 H4: a P2P registration at an address another P2P Node holds (the DevEUI's low
        /// 32 bits) is refused before anything is signed, naming both Nodes and the address.
        #[tokio::test]
        async fn a_p2p_registration_on_a_held_address_is_refused_naming_both() {
            use control_radio::Mode;
            const APP_KEY: &str = "0123456789abcdef0123456789abcdef";
            const HOLDER: &str = "70b3d57ed0000abe";
            const NEW: &str = "58760700d0000abe";
            let (mut state, _dir) = test_state().await;
            let radio_dir = tempfile::tempdir().expect("tempdir");
            state.radio = crate::radio::RadioControl::for_tests(Mode::Dual, radio_dir.path());
            crate::radio::test_support::register(
                radio_dir.path(),
                portal_domain::Transport::P2p,
                HOLDER,
            );
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            let html = body_text(
                post(
                    &router,
                    "/nodes",
                    &manager,
                    &format!("serial=42&dev_eui={NEW}&app_key={APP_KEY}&radio=p2p"),
                )
                .await,
            )
            .await;
            let sentence = tr("monitoring.nodes.err_address_collision")
                .replace("{dev_eui}", &NEW.to_ascii_uppercase())
                .replace("{addr}", "D0000ABE")
                .replace("{holder_dev_eui}", &HOLDER.to_ascii_uppercase());
            assert!(html.contains(&esc(&sentence)), "{html}");
            assert!(state.relay.published().is_empty(), "nothing signed");
        }

        /// #470 H4: the Hub's own `AddressCollision` (a registry the Portal could not see)
        /// becomes the pre-check's plain sentence, built from the DevEUIs the Hub's line names;
        /// a line that does not parse leads with the generic sentence and keeps the Hub's
        /// words; any other refusal stays the Hub's words alone.
        #[test]
        fn a_hub_refused_registration_leads_with_the_collision_sentence() {
            use crate::relay::{error_code, CommandOutcome};
            // The line the bench Hub returned on 2026-10-07 (c69).
            let hub = "AddressCollision [p2p/node-registry]: dev_eui 70b3d57e00005722 has the \
                       P2P address 00005722 (its low 32 bits), which 5876070000005722 already \
                       holds on p2p; the Hub's P2P radio cannot serve both — keep one of them \
                       on LoRaWAN";
            let outcome = |code, message: Option<&str>| CommandOutcome {
                request_id: None,
                command_type: "node-add".into(),
                status: "error",
                message: message.map(str::to_string),
                error_code: Some(code),
                result: None,
            };
            assert_eq!(
                super::super::add_failure_reason(&outcome(
                    error_code::ADDRESS_COLLISION,
                    Some(hub)
                )),
                tr("monitoring.nodes.err_address_collision")
                    .replace("{dev_eui}", "70B3D57E00005722")
                    .replace("{addr}", "00005722")
                    .replace("{holder_dev_eui}", "5876070000005722")
            );
            let odd = "AddressCollision [p2p/node-registry]: reworded";
            let reason = super::super::add_failure_reason(&outcome(
                error_code::ADDRESS_COLLISION,
                Some(odd),
            ));
            assert!(
                reason.starts_with(&tr("monitoring.command_error.address_collision")),
                "{reason}"
            );
            assert!(reason.contains(odd), "{reason}");
            assert_eq!(
                super::super::add_failure_reason(&outcome(
                    error_code::DEVICE_REJECTED,
                    Some("legacy LoRaWAN stack disabled")
                )),
                "legacy LoRaWAN stack disabled"
            );
            assert_eq!(
                super::super::add_failure_reason(&outcome(error_code::DEVICE_REJECTED, None)),
                tr("monitoring.nodes.err_add_failed")
            );
        }

        /// PN-8, on one registry: a removal goes to the one carrier the registry holds the Node
        /// on, whatever the Hub's mode, and the audit row says which. Both carriers.
        #[tokio::test]
        async fn a_removal_reaches_the_one_carrier_the_node_is_on() {
            use control_radio::Mode;
            for (carrier, mode) in [
                (portal_domain::Transport::P2p, Mode::P2p),
                (portal_domain::Transport::Lorawan, Mode::Lorawan),
                (portal_domain::Transport::Lorawan, Mode::Dual),
                (portal_domain::Transport::P2p, Mode::Dual),
            ] {
                let (mut state, _dir) = test_state().await;
                let radio_dir = tempfile::tempdir().expect("tempdir");
                state.radio = crate::radio::RadioControl::for_tests(mode, radio_dir.path());
                crate::radio::test_support::register(radio_dir.path(), carrier.other(), NODE);
                // A second registration moves it: one registry, one carrier.
                crate::radio::test_support::register(radio_dir.path(), carrier, NODE);
                let admin = login_as_role(&state, Role::Admin, "admin1").await;
                seed_node(&state, NODE);
                pair_hub(&state).await;
                let router = super::super::router().with_state(state.clone());

                let response = with_signed_success(&state, move || async move {
                    post(
                        &router,
                        &format!("/nodes/{NODE}/remove"),
                        &admin,
                        &format!("confirm={NODE}"),
                    )
                    .await
                })
                .await;
                let case = format!("{carrier:?} on {mode:?}");
                assert_eq!(
                    response
                        .headers()
                        .get(header::LOCATION)
                        .and_then(|v| v.to_str().ok()),
                    Some("/nodes?ok=removed"),
                    "{case}"
                );
                assert_eq!(
                    published_radios(&state, "node-remove"),
                    vec![carrier.as_str().to_string()],
                    "{case}"
                );
                let details = audit_details(&state, "node_remove").await;
                assert_eq!(details["transport"], carrier.as_str(), "{case}: {details}");
            }
        }

        /// The "registered on both carriers" fault of step P1 is gone with the state it
        /// reported (node_registry.md §6): no banner, no string for one, on a Node the registry
        /// moved from one carrier to the other.
        #[tokio::test]
        async fn the_both_carriers_fault_is_gone() {
            use control_radio::Mode;
            let (mut state, _dir) = test_state().await;
            let radio_dir = tempfile::tempdir().expect("tempdir");
            state.radio = crate::radio::RadioControl::for_tests(Mode::Dual, radio_dir.path());
            for carrier in portal_domain::Transport::ALL {
                crate::radio::test_support::register(radio_dir.path(), carrier, NODE);
            }
            let admin = login_as_role(&state, Role::Admin, "admin1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());
            let html =
                body_text(get(&router, &format!("/nodes/{NODE}/overview"), &admin).await).await;
            for key in [
                "monitoring.node_detail.carrier_conflict",
                "monitoring.node_detail.carrier_conflict_help",
            ] {
                assert_eq!(tr(key), key, "{key} is no longer a string");
                assert!(!html.contains(key), "{key}");
            }
            assert!(!html.contains("both carriers"), "no fault banner");
        }

        /// `text` as the page's autoescape renders it, to find a translated sentence in HTML.
        fn esc(text: impl AsRef<str>) -> String {
            minijinja::HtmlEscape(text.as_ref()).to_string()
        }

        // ---- R6: "Změnit rádio" (node_registry.md §6) ------------------

        /// The whole `<input …>` tag carrying `needle`, to read its attributes whatever the
        /// template's line breaks.
        fn input_tag(html: &str, needle: &str) -> String {
            let at = html
                .find(needle)
                .unwrap_or_else(|| panic!("no input with {needle}"));
            let start = html[..at].rfind('<').expect("tag start");
            let end = html[at..].find('>').expect("tag end") + at;
            html[start..=end].to_string()
        }

        /// A Hub in `mode` whose registry holds [`NODE`] on `carrier`, an admin and a manager.
        async fn move_fixture(
            mode: control_radio::Mode,
            carrier: portal_domain::Transport,
        ) -> (
            AppState,
            tempfile::TempDir,
            tempfile::TempDir,
            axum::Router,
            String,
            String,
        ) {
            let (mut state, dir) = test_state().await;
            let radio_dir = tempfile::tempdir().expect("tempdir");
            state.radio = crate::radio::RadioControl::for_tests(mode, radio_dir.path());
            crate::radio::test_support::register(radio_dir.path(), carrier, NODE);
            let admin = login_as_role(&state, Role::Admin, "admin1").await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());
            (state, dir, radio_dir, router, admin, manager)
        }

        /// #470 H4: a move to P2P onto an address another P2P Node holds is refused before
        /// anything is signed, naming both Nodes.
        #[tokio::test]
        async fn a_move_to_p2p_onto_a_held_address_is_refused_naming_both() {
            let (state, _dir, radio, router, admin, _manager) =
                move_fixture(control_radio::Mode::Dual, portal_domain::Transport::Lorawan).await;
            // NODE's low 32 bits under another DevEUI, on P2P.
            let holder = format!("aabbccdd{}", &NODE[8..]);
            crate::radio::test_support::register(
                radio.path(),
                portal_domain::Transport::P2p,
                &holder,
            );
            let html = body_text(
                post(
                    &router,
                    &format!("/nodes/{NODE}/move"),
                    &admin,
                    &format!("to=p2p&confirm={NODE}"),
                )
                .await,
            )
            .await;
            let sentence = tr("monitoring.nodes.err_address_collision")
                .replace("{dev_eui}", &NODE.to_ascii_uppercase())
                .replace("{addr}", &NODE[8..].to_ascii_uppercase())
                .replace("{holder_dev_eui}", &holder.to_ascii_uppercase());
            assert!(html.contains(&esc(&sentence)), "{html}");
            assert!(state.relay.published().is_empty(), "nothing signed");
        }

        /// Admin only, like removal: a manager — who registers Nodes — is refused before
        /// anything is signed, and is shown neither the card nor the dialog.
        #[tokio::test]
        async fn changing_a_nodes_radio_is_admin_only() {
            let (state, _dir, _radio, router, admin, manager) =
                move_fixture(control_radio::Mode::Dual, portal_domain::Transport::P2p).await;
            let response = post(
                &router,
                &format!("/nodes/{NODE}/move"),
                &manager,
                &format!("to=lorawan&confirm={NODE}"),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert!(state.relay.published().is_empty());

            let path = format!("/nodes/{NODE}/settings");
            let as_manager = body_text(get(&router, &path, &manager).await).await;
            assert!(!as_manager.contains("id=\"node-move\""), "no dialog");
            assert!(!as_manager.contains(&esc(tr("monitoring.node_detail.move_button"))));
            let as_admin = body_text(get(&router, &path, &admin).await).await;
            assert!(as_admin.contains("id=\"node-move\""), "the dialog");
            assert!(as_admin.contains(&esc(tr("monitoring.node_detail.move_button"))));
            // The dialog says plainly that the Node has to be switched too (§10 Q1).
            assert!(
                as_admin.contains(&esc(tr("monitoring.node_detail.move_node_warning"))),
                "the NFC / radio-mode sentence"
            );
            // The target is the other carrier, the current one shown and not choosable.
            let target = input_tag(&as_admin, "name=\"to\" value=\"lorawan\"");
            assert!(target.contains("checked"), "{target}");
            assert!(!target.contains("disabled"), "{target}");
            assert!(input_tag(&as_admin, "name=\"to\" value=\"p2p\"").contains("disabled"));
        }

        /// The typed confirm is the DevEUI, checked on the server. A mismatch sends nothing and
        /// is audited as a refusal; so is nothing for a target that is the Node's own carrier.
        #[tokio::test]
        async fn a_move_needs_the_typed_confirm_and_another_carrier() {
            let (state, _dir, _radio, router, admin, _) =
                move_fixture(control_radio::Mode::Dual, portal_domain::Transport::P2p).await;

            let html = body_text(
                post(
                    &router,
                    &format!("/nodes/{NODE}/move"),
                    &admin,
                    "to=lorawan&confirm=70b3d5e050019999",
                )
                .await,
            )
            .await;
            assert!(
                html.contains(&esc(tr("monitoring.node_detail.err_move_confirm_mismatch"))),
                "{html}"
            );
            assert!(state.relay.published().is_empty(), "nothing signed");
            let details = audit_details(&state, "node_move").await;
            assert_eq!(details["refused"], "confirmation_mismatch", "{details}");

            for body in [
                format!("to=p2p&confirm={NODE}"),
                format!("to=wifi&confirm={NODE}"),
            ] {
                let html =
                    body_text(post(&router, &format!("/nodes/{NODE}/move"), &admin, &body).await)
                        .await;
                assert!(html.contains("alert"), "{body}: {html}");
            }
            assert!(state.relay.published().is_empty(), "nothing signed");
        }

        /// The move itself: the signed `node-move` `{dev_eui, to}` at level `admin` through the
        /// relay `node-remove` takes, an audit row `node_move` with `from` and `to`, and an
        /// `HX-Redirect` to the settings tab — which then shows the new carrier from the
        /// registry the Hub flipped, and the reminder to switch the Node itself.
        #[tokio::test]
        async fn a_move_sends_the_signed_command_audits_it_and_shows_the_new_carrier() {
            let (state, _dir, radio_dir, router, admin, _) =
                move_fixture(control_radio::Mode::Dual, portal_domain::Transport::P2p).await;

            let (r, a) = (router.clone(), admin.clone());
            let response = with_signed_success(&state, move || async move {
                post(
                    &r,
                    &format!("/nodes/{NODE}/move"),
                    &a,
                    &format!("to=lorawan&confirm={}", NODE.to_ascii_uppercase()),
                )
                .await
            })
            .await;
            assert_eq!(
                response
                    .headers()
                    .get("HX-Redirect")
                    .and_then(|v| v.to_str().ok()),
                Some(&*format!("/nodes/{NODE}/settings?ok=moved"))
            );

            let published = state.relay.published();
            let (_, body) = published
                .iter()
                .find(|(topic, _)| topic.ends_with("commands/config/request"))
                .expect("the signed request");
            assert_eq!(body["command_type"], "node-move");
            assert_eq!(body["level"], "admin");
            assert_eq!(
                body["params"],
                serde_json::json!({ "dev_eui": NODE, "to": "lorawan" })
            );

            let details = audit_details(&state, "node_move").await;
            assert_eq!(details["from"], "p2p", "{details}");
            assert_eq!(details["to"], "lorawan", "{details}");
            assert_eq!(details["status"], "success", "{details}");
            let row = audit_rows(&state)
                .await
                .into_iter()
                .find(|row| row.event_type == "node_move")
                .expect("audited");
            assert!(row.success);

            // What the Hub's node-move did to its registry, and the page after the redirect.
            crate::radio::test_support::register(
                radio_dir.path(),
                portal_domain::Transport::Lorawan,
                NODE,
            );
            let html =
                body_text(get(&router, &format!("/nodes/{NODE}/settings?ok=moved"), &admin).await)
                    .await;
            let moved = tr("monitoring.node_detail.moved_ok")
                .replace("{carrier}", &tr("monitoring.transport.lorawan"));
            assert!(html.contains(&esc(&moved)), "{html}");
            assert!(
                html.contains(
                    &esc(tr("monitoring.node_detail.radio_current"))
                        .replace("{carrier}", &tr("monitoring.transport.lorawan"))
                ),
                "the radio card names the new carrier"
            );
        }

        /// A move the Hub does not confirm (here the test relay's silence) renders its outcome
        /// into `#command-status` instead of redirecting, and is audited as failed.
        #[tokio::test(start_paused = true)]
        async fn a_move_the_hub_does_not_confirm_is_shown_and_audited_as_failed() {
            let (state, _dir, _radio, router, admin, _) =
                move_fixture(control_radio::Mode::Dual, portal_domain::Transport::Lorawan).await;
            let response = post(
                &router,
                &format!("/nodes/{NODE}/move"),
                &admin,
                &format!("to=p2p&confirm={NODE}"),
            )
            .await;
            assert!(response.headers().get("HX-Redirect").is_none());
            let html = body_text(response).await;
            assert!(html.contains("id=\"command-status\""), "{html}");
            let row = audit_rows(&state)
                .await
                .into_iter()
                .find(|row| row.event_type == "node_move")
                .expect("audited");
            assert!(!row.success);
            let details: serde_json::Value =
                serde_json::from_str(row.details.as_deref().unwrap_or("{}")).expect("json");
            assert_eq!(details["from"], "lorawan");
            assert_eq!(details["to"], "p2p");
        }

        /// node_registry.md §4, §11: a Node the registry keeps on a carrier this Hub has
        /// switched off keeps R7's banner on every tab, the badge names the registry's carrier,
        /// and an admin gets "Změnit rádio" in the banner. A manager sees the banner only.
        #[tokio::test]
        async fn a_node_on_a_switched_off_carrier_offers_the_move_beside_the_banner() {
            for (mode, carrier) in [
                (control_radio::Mode::P2p, portal_domain::Transport::Lorawan),
                (control_radio::Mode::Lorawan, portal_domain::Transport::P2p),
            ] {
                let (_state, _dir, _radio, router, admin, manager) =
                    move_fixture(mode, carrier).await;
                let case = format!("{carrier:?} on {mode:?}");
                let label = tr(&format!("monitoring.transport.{}", carrier.as_str()));
                let banner =
                    esc(tr("monitoring.node_detail.carrier_off").replace("{carrier}", &label));
                for tab in ["overview", "settings", "history", "diagnostics"] {
                    let html =
                        body_text(get(&router, &format!("/nodes/{NODE}/{tab}"), &admin).await)
                            .await;
                    let at = html
                        .find(&banner)
                        .unwrap_or_else(|| panic!("{case} {tab}: the banner"));
                    // The action sits in the banner: the next button after it opens the panel.
                    let rest = &html[at..];
                    let opens = rest.find("onclick=").expect("a button after the banner");
                    assert!(
                        rest[opens..].contains("getElementById('node-move')"),
                        "{case} {tab}"
                    );
                    assert!(
                        html.contains("id=\"node-move\""),
                        "{case} {tab}: the dialog"
                    );
                    assert!(
                        html.contains(&format!(
                            "<span class=\"badge-outline-v1\">{}</span>",
                            esc(&label)
                        )),
                        "{case} {tab}: the registry's carrier on the badge"
                    );
                }
                let html =
                    body_text(get(&router, &format!("/nodes/{NODE}/overview"), &manager).await)
                        .await;
                assert!(html.contains(&banner), "{case}: a manager sees the state");
                assert!(!html.contains("id=\"node-move\""), "{case}: not the action");
            }
        }

        // ---- R6: the registration form's radio choice ------------------

        /// devices.node.md, dodatek 2026-09-27: the dialog's radio choice is preselected from
        /// `radio.mode` (P2P on `dual`, Hynek 2026-09-27), and a carrier this Hub has switched off is shown
        /// disabled with a hint. The JoinEUI field is LoRaWAN's only.
        #[tokio::test]
        async fn the_registration_dialog_preselects_the_carrier_by_mode() {
            use control_radio::Mode;
            for (mode, checked, disabled) in [
                (Mode::Lorawan, "lorawan", Some("p2p")),
                (Mode::P2p, "p2p", Some("lorawan")),
                (Mode::Dual, "p2p", None),
            ] {
                let (mut state, _dir) = test_state().await;
                let radio_dir = tempfile::tempdir().expect("tempdir");
                state.radio = crate::radio::RadioControl::for_tests(mode, radio_dir.path());
                let manager = login_as_role(&state, Role::Manager, "manager1").await;
                seed_node(&state, NODE);
                let router = super::super::router().with_state(state.clone());
                let html = body_text(get(&router, "/nodes/add", &manager).await).await;
                let radio_input =
                    |c: &str| input_tag(&html, &format!("name=\"radio\" value=\"{c}\""));
                assert!(radio_input(checked).contains("checked"), "{mode:?}");
                assert!(!radio_input(checked).contains("disabled"), "{mode:?}");
                assert!(html.contains(&format!("x-data=\"{{ radio: '{checked}' }}\"")));
                match disabled {
                    Some(off) => {
                        assert!(radio_input(off).contains("disabled"), "{mode:?}");
                        assert!(
                            html.contains(
                                &esc(tr("monitoring.nodes.radio_off_hint")).replace(
                                    "{carrier}",
                                    &tr(&format!("monitoring.transport.{off}"))
                                )
                            ),
                            "{mode:?}: the hint"
                        );
                    }
                    None => {
                        for c in ["lorawan", "p2p"] {
                            assert!(!radio_input(c).contains("disabled"), "{mode:?} {c}");
                        }
                    }
                }
                assert_eq!(
                    html.contains("name=\"join_eui\""),
                    mode != Mode::P2p,
                    "{mode:?}: JoinEUI only where LoRaWAN is on"
                );
            }
        }

        /// The chosen carrier goes on the wire; one this Hub has switched off is refused before
        /// anything is signed, naming it; and the JoinEUI rides a LoRaWAN registration only.
        #[tokio::test(start_paused = true)]
        async fn a_registration_goes_out_on_the_chosen_carrier_if_it_is_on() {
            use control_radio::Mode;
            const APP_KEY: &str = "0123456789abcdef0123456789abcdef";
            for (mode, chosen, sent) in [
                (Mode::Dual, "p2p", Some("p2p")),
                (Mode::Dual, "lorawan", Some("lorawan")),
                (Mode::P2p, "lorawan", None),
                (Mode::Lorawan, "p2p", None),
            ] {
                let (mut state, _dir) = test_state().await;
                let radio_dir = tempfile::tempdir().expect("tempdir");
                state.radio = crate::radio::RadioControl::for_tests(mode, radio_dir.path());
                let manager = login_as_role(&state, Role::Manager, "manager1").await;
                seed_node(&state, NODE);
                pair_hub(&state).await;
                let router = super::super::router().with_state(state.clone());
                let response = post(
                    &router,
                    "/nodes",
                    &manager,
                    &format!(
                        "radio={chosen}&serial=42&dev_eui=70b3d5e05001aaaa&app_key={APP_KEY}\
                         &join_eui=70b3d57ed0000001"
                    ),
                )
                .await;
                let case = format!("{chosen} on {mode:?}");
                match sent {
                    Some(carrier) => {
                        let published = state.relay.published();
                        let (_, body) = published.first().expect("published");
                        assert_eq!(body["params"]["radio"], carrier, "{case}");
                        assert_eq!(
                            body["params"].get("join_eui").is_some(),
                            carrier == "lorawan",
                            "{case}"
                        );
                    }
                    None => {
                        let code = format!("radio_off_{chosen}");
                        assert_eq!(
                            response
                                .headers()
                                .get(header::LOCATION)
                                .and_then(|v| v.to_str().ok()),
                            Some(&*format!("/nodes?error={code}")),
                            "{case}"
                        );
                        assert!(state.relay.published().is_empty(), "{case}");
                        assert!(
                            super::super::node_list_flash(&code)
                                .contains(&tr(&format!("monitoring.transport.{chosen}"))),
                            "{case}"
                        );
                    }
                }
            }
        }

        /// node_registry.md §2: a serial is a u32, typed in decimal or hex. One that is neither,
        /// or past 32 bits, is refused before anything is signed.
        #[tokio::test(start_paused = true)]
        async fn a_serial_in_decimal_or_hex_is_taken_and_one_past_u32_is_refused() {
            const APP_KEY: &str = "0123456789abcdef0123456789abcdef";
            for (serial, taken) in [
                ("2162165722", true),
                ("80e007da", true),
                ("0x80E007DA", true),
                ("4294967296", false),
                ("SN-1", false),
                ("80e007", false),
            ] {
                let (state, _dir) = test_state().await;
                let manager = login_as_role(&state, Role::Manager, "manager1").await;
                seed_node(&state, NODE);
                pair_hub(&state).await;
                let router = super::super::router().with_state(state.clone());
                let response = post(
                    &router,
                    "/nodes",
                    &manager,
                    &format!("serial={serial}&dev_eui=70b3d5e05001aaaa&app_key={APP_KEY}"),
                )
                .await;
                if taken {
                    let published = state.relay.published();
                    let (_, body) = published.first().expect("published");
                    assert_eq!(body["params"]["serial"], serial, "{serial}");
                } else {
                    assert_eq!(
                        response
                            .headers()
                            .get(header::LOCATION)
                            .and_then(|v| v.to_str().ok()),
                        Some("/nodes?error=invalid_serial"),
                        "{serial}"
                    );
                    assert!(state.relay.published().is_empty(), "{serial}");
                }
            }
        }

        /// node_registry.md §11: after the Hub's radio changes, a Node registered on the carrier
        /// that went off stays, and its page says its radio is off on this Hub. On a Hub that
        /// runs its carrier the page says nothing of the kind.
        #[tokio::test]
        async fn a_node_on_a_switched_off_carrier_says_so() {
            use control_radio::Mode;
            for (mode, off) in [
                (Mode::P2p, true),
                (Mode::Lorawan, false),
                (Mode::Dual, false),
            ] {
                let (mut state, _dir) = test_state().await;
                let radio_dir = tempfile::tempdir().expect("tempdir");
                state.radio = crate::radio::RadioControl::for_tests(mode, radio_dir.path());
                crate::radio::test_support::register(
                    radio_dir.path(),
                    portal_domain::Transport::Lorawan,
                    NODE,
                );
                let admin = login_as_role(&state, Role::Admin, "admin1").await;
                seed_node(&state, NODE);
                let router = super::super::router().with_state(state.clone());
                let html =
                    body_text(get(&router, &format!("/nodes/{NODE}/overview"), &admin).await).await;
                let banner = tr("monitoring.node_detail.carrier_off")
                    .replace("{carrier}", &tr("monitoring.transport.lorawan"))
                    .replace('\'', "&#x27;");
                assert_eq!(html.contains(&banner), off, "{mode:?}: {html}");
            }
        }

        /// WP4 on combined20: a registration the Hub does not accept (here the test relay's
        /// silence, a timeout) renders the Nodes list with the reason in its error flash. It
        /// used to answer the bare `#command-status` fragment, which the dialog's plain form
        /// POST painted over the whole page.
        #[tokio::test(start_paused = true)]
        async fn a_refused_registration_lands_on_the_list_page() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            let html = body_text(
                post(
                    &router,
                    "/nodes",
                    &manager,
                    "serial=42&dev_eui=70b3d5e05001aaaa&app_key=0123456789abcdef0123456789abcdef",
                )
                .await,
            )
            .await;
            assert!(
                html.contains("<html"),
                "a whole page, not a fragment: {html}"
            );
            assert!(html.contains("data-list-action"), "the Nodes list: {html}");
            assert!(
                html.contains("alert-error"),
                "with the refusal flashed: {html}"
            );
            assert!(
                !html.starts_with("<div id=\"command-status\""),
                "not the bare status fragment"
            );
        }

        /// #86: a typed JoinEUI rides lowercased; a cleared field puts nothing
        /// on the wire, which is what makes the Hub apply `radio.join_eui` —
        /// for this agent and for one that predates the param.
        #[tokio::test(start_paused = true)]
        async fn the_join_eui_rides_when_typed_and_stays_off_the_wire_when_blank() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            for (dev_eui, join_eui) in [
                ("70b3d5e05001aaaa", "00000000000000AB"),
                ("70b3d5e05001bbbb", ""),
            ] {
                post(
                    &router,
                    "/nodes",
                    &manager,
                    &format!(
                        "serial=42&dev_eui={dev_eui}&app_key=0123456789abcdef0123456789abcdef&join_eui={join_eui}"
                    ),
                )
                .await;
            }

            let published = state.relay.published();
            let params: Vec<&serde_json::Value> =
                published.iter().map(|(_, body)| &body["params"]).collect();
            assert_eq!(params.len(), 2, "{published:?}");
            assert_eq!(params[0]["join_eui"], "00000000000000ab");
            assert!(params[1].get("join_eui").is_none(), "{}", params[1]);
        }

        /// #86: the dialog is prefilled with the Hub default the agent
        /// publishes in `system/info.lorawan.join_eui`.
        #[tokio::test]
        async fn the_register_dialog_prefills_the_hubs_default_join_eui() {
            use fiber_protocol::payloads::system_info::{LorawanGatewayInfo, SystemInfoMsg};

            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            state.store.apply(IngestEvent::SystemInfo {
                hostname: HOST.into(),
                payload: Box::new(SystemInfoMsg {
                    lorawan: Some(LorawanGatewayInfo {
                        join_eui: Some("5876070000000000".into()),
                        ..LorawanGatewayInfo::default()
                    }),
                    ..SystemInfoMsg::default()
                }),
            });
            let router = super::super::router().with_state(state.clone());

            let page = body_text(get(&router, "/nodes/add", &manager).await).await;
            assert!(
                page.contains(r#"name="join_eui""#) && page.contains(r#"value="5876070000000000""#),
                "the JoinEUI field must be prefilled with the Hub default"
            );
            // Cleared, the field still shows what blank means.
            assert!(
                page.contains(r#"placeholder="5876070000000000""#),
                "the Hub default must also be the field's placeholder"
            );
        }

        /// §19's F7 rule, extended to the draft: both are keyed by the
        /// DevEUI and a DevEUI is reused, so both go with the device. A
        /// draft that outlived its Node would be sent to its replacement.
        #[tokio::test]
        /// `remove_lorawan_sticker` is `destructive`
        /// (plan/portal/mqtt/design.md §4.2) and the route is `RequireAdmin`
        /// to match, so the actor is an admin — `a_manager_is_refused_a_node_removal`
        /// below is the other half.
        async fn removing_a_node_drops_its_device_meta_and_its_draft() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Admin, "admin1").await;
            seed_node(&state, NODE);
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            post(
                &router,
                &format!("/nodes/{NODE}/meta"),
                &manager,
                "description=cold+room&offline_k=",
            )
            .await;
            post(
                &router,
                &format!("/nodes/{NODE}/config"),
                &manager,
                "key=application.interval_report&value=600",
            )
            .await;
            assert_eq!(
                state.pending.list(HOST, NODE).await.expect("draft").len(),
                1
            );

            let removal_router = router.clone();
            let removal_token = manager.clone();
            let response = with_signed_success(&state, move || async move {
                post(
                    &removal_router,
                    &format!("/nodes/{NODE}/remove"),
                    &removal_token,
                    &format!("confirm={NODE}"),
                )
                .await
            })
            .await;
            assert_eq!(
                response.status(),
                StatusCode::SEE_OTHER,
                "a successful removal redirects to the list"
            );
            assert_eq!(
                response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/nodes?ok=removed")
            );

            assert!(
                ({
                    let a0 = HOST.to_string();
                    let a1 = portal_storage::THRESHOLD_SCOPE_NODE.to_string();
                    let a2 = NODE.to_string();
                    state.db.run(move |s| s.get_device_meta(&a0, &a1, &a2))
                })
                .await
                .expect("meta read")
                .is_none(),
                "device_meta goes with the device"
            );
            assert!(
                state
                    .pending
                    .list(HOST, NODE)
                    .await
                    .expect("draft")
                    .is_empty(),
                "and so does the draft"
            );
        }

        /// Removal is destructive and irreversible, so its confirmation is
        /// typed and checked here rather than trusted from the browser.
        ///
        /// Driven as an admin: `remove_lorawan_sticker` is `destructive`
        /// (plan/portal/mqtt/design.md §4.2) and the route is `RequireAdmin`
        /// to match, so a manager would never reach the confirmation check
        /// this test is about — see `a_manager_is_refused_a_node_removal`.
        #[tokio::test]
        async fn removal_refuses_a_mismatched_confirmation() {
            let (state, _dir) = test_state().await;
            let admin = login_as_role(&state, Role::Admin, "admin1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                &format!("/nodes/{NODE}/remove"),
                &admin,
                "confirm=whatever",
            )
            .await;
            assert_eq!(
                response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some(&*format!("/nodes/{NODE}/overview?error=confirm_mismatch"))
            );
            assert!(state.relay.published().is_empty());

            // UX-044: the page the refusal lands on says so.
            let page = body_text(
                get(
                    &router,
                    &format!("/nodes/{NODE}/overview?error=confirm_mismatch"),
                    &admin,
                )
                .await,
            )
            .await;
            assert!(
                page.contains("That is not this Node&#x27;s key.")
                    || page.contains("That is not this Node's key."),
                "no refusal on the page"
            );
            assert!(
                !page.contains(&tr("monitoring.meta.err_save_failed")),
                "and not as a metadata failure"
            );
        }

        /// GitLab #109: a Node registration or removal on a Hub that is not paired is refused
        /// before anything is signed, and the page it lands on names the pairing — it said
        /// "could not be registered / removed". Not audited: nothing was sent.
        #[tokio::test]
        async fn an_unpaired_hub_refuses_node_add_and_removal_as_not_paired() {
            let (state, _dir) = test_state().await;
            let admin = login_as_role(&state, Role::Admin, "admin1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());
            let not_paired = tr("monitoring.shared.signing_key_not_paired");
            let location = |response: &axum::response::Response| {
                response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };

            let added = post(
                &router,
                "/nodes",
                &admin,
                "serial=7&dev_eui=70b3d5e05001bbbb&app_key=00000000000000000000000000000000",
            )
            .await;
            assert_eq!(location(&added).as_deref(), Some("/nodes?error=not_paired"));
            let page = body_text(get(&router, "/nodes?error=not_paired", &admin).await).await;
            assert!(page.contains(&not_paired), "the list says why");
            assert!(!page.contains(&tr("monitoring.nodes.err_add_failed")));

            let removed = post(
                &router,
                &format!("/nodes/{NODE}/remove"),
                &admin,
                &format!("confirm={NODE}"),
            )
            .await;
            assert_eq!(
                location(&removed),
                Some(format!("/nodes/{NODE}/overview?error=not_paired"))
            );
            let page = body_text(
                get(
                    &router,
                    &format!("/nodes/{NODE}/overview?error=not_paired"),
                    &admin,
                )
                .await,
            )
            .await;
            assert!(page.contains(&not_paired), "the Node's page says why");
            assert!(!page.contains(&tr("monitoring.meta.err_save_failed")));

            assert!(state.relay.published().is_empty(), "nothing was signed");
            assert!(
                audit_rows(&state)
                    .await
                    .iter()
                    .all(|r| r.event_type != "node_add" && r.event_type != "node_remove"),
                "a refusal before signing is not an action"
            );
        }

        /// #109, said up front: while the Hub is not paired `/nodes` names it above the
        /// Register button, and the button and the dialog's submit are disabled. Paired, both
        /// are back and the banner is gone.
        #[tokio::test]
        async fn the_register_button_waits_for_the_hub_to_be_paired() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());
            let register = tr("monitoring.nodes.register_button");
            let disabled_submit = format!(
                r#"<button type="submit" class="btn-v1 btn-v1-primary" disabled>{register}</button>"#
            );
            let opener = |page: &str| -> String {
                page.split("data-list-action")
                    .nth(1)
                    .and_then(|rest| rest.split('>').next())
                    .expect("the Register button")
                    .to_string()
            };

            let page = body_text(get(&router, "/nodes", &manager).await).await;
            assert!(page.contains(r#"data-unpaired-hub="h1""#), "{page}");
            assert!(opener(&page).contains(" disabled"), "{page}");
            let add = body_text(get(&router, "/nodes/add", &manager).await).await;
            assert!(add.contains(&disabled_submit), "{add}");

            pair_hub(&state).await;
            let page = body_text(get(&router, "/nodes", &manager).await).await;
            assert!(!page.contains("data-unpaired-hub"), "{page}");
            assert!(!opener(&page).contains(" disabled"), "{page}");
            let add = body_text(get(&router, "/nodes/add", &manager).await).await;
            assert!(!add.contains(&disabled_submit), "{add}");
        }

        /// The twin of `beacon_routes.rs`'s
        /// `a_manager_is_refused_a_beacon_removal`, and the test
        /// `removing_a_node_drops_its_device_meta_and_its_draft` above has
        /// been pointing at since P2 opened.
        ///
        /// `remove_lorawan_sticker` is `destructive` (plan/portal/mqtt/design.md
        /// §4.2, §13 item 3 ratified 2026-09-15), so the refusal is the
        /// route's and lands before the handler does anything: no
        /// confirmation check, no ChirpStack deprovisioning, no
        /// `device_meta` or draft deleted, nothing signed. A manager turned
        /// back *after* the handler had run would be the same half-success
        /// the Beacon route used to have.
        #[tokio::test]
        async fn a_manager_is_refused_a_node_removal() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            seed_node(&state, NODE);
            pair_hub(&state).await;
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                &format!("/nodes/{NODE}/remove"),
                &manager,
                // The confirmation is correct, so nothing but the role can
                // be what refuses this.
                &format!("confirm={NODE}"),
            )
            .await;

            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "refused at the gate, not after the handler has run"
            );
            assert!(
                state.relay.published().is_empty(),
                "nothing reaches the relay's tape"
            );
            let (_, total) = state
                .auth
                .audit_page(portal_storage::AuditFilter {
                    event_type: Some("node_remove".to_string()),
                    ..Default::default()
                })
                .await
                .expect("audit page");
            assert_eq!(total, 0, "and the handler never ran to write a row");
        }

        /// A reveal is admin-only and leaves a trail whatever it reveals —
        /// which today is nothing, because the Hub publishes none of these
        /// values yet. The audit is the part that has to exist now.
        #[tokio::test]
        async fn revealing_a_key_is_admin_only_and_audited() {
            let (state, _dir) = test_state().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let admin = login(&state, "admin", "correct-horse-battery").await;
            seed_node(&state, NODE);
            let router = super::super::router().with_state(state.clone());

            assert_eq!(
                post(&router, &format!("/nodes/{NODE}/keys/reveal"), &manager, "")
                    .await
                    .status(),
                StatusCode::FORBIDDEN
            );
            assert!(
                !audit_rows(&state)
                    .await
                    .iter()
                    .any(|r| r.event_type == "node_key_reveal"),
                "a refused reveal writes nothing"
            );

            let response = post(&router, &format!("/nodes/{NODE}/keys/reveal"), &admin, "").await;
            assert_eq!(response.status(), StatusCode::OK);
            let html = body_text(response).await;
            assert!(html.contains("node-keys-revealed"));

            let row = audit_rows(&state)
                .await
                .into_iter()
                .find(|r| r.event_type == "node_key_reveal")
                .expect("audited");
            assert_eq!(row.username.as_deref(), Some("admin"));
            let details: serde_json::Value =
                serde_json::from_str(row.details.as_deref().expect("details")).expect("json");
            assert_eq!(
                details["revealed"],
                serde_json::json!([]),
                "nothing is stored yet, and the row says so honestly"
            );
        }

        /// The `no_device_yet` early return has to come first in every
        /// handler: `MUTATING_ROUTES`' role sweep POSTs to each of them, and
        /// a handler that reaches the relay before answering makes the sweep
        /// wait out the 20 s command timeout per route.
        #[tokio::test]
        async fn every_b4_route_answers_immediately_with_no_hub() {
            let (state, _dir) = test_state().await;
            let admin = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state.clone());

            let started = std::time::Instant::now();
            for (path, body) in [
                (
                    format!("/nodes/{NODE}/config"),
                    "key=application.interval_report&value=600",
                ),
                (
                    format!("/nodes/{NODE}/thresholds/temperature"),
                    "critical_low=&warning_low=&warning_high=&critical_high=8",
                ),
                (format!("/nodes/{NODE}/pending/send"), ""),
                (format!("/nodes/{NODE}/pending/discard"), ""),
                (format!("/nodes/{NODE}/read-device"), ""),
                (format!("/nodes/{NODE}/service/force-send"), ""),
                (format!("/nodes/{NODE}/service/send-msg"), "payload=%7B%7D"),
                (
                    format!("/nodes/{NODE}/service/reboot"),
                    &format!("confirm={NODE}"),
                ),
                (format!("/nodes/{NODE}/remove"), &format!("confirm={NODE}")),
                (
                    "/nodes".to_string(),
                    "serial=1&dev_eui=70b3d5e05001aaaa&app_key=00000000000000000000000000000000",
                ),
            ] {
                let status = post(&router, &path, &admin, body).await.status();
                assert!(
                    status.is_success() || status.is_redirection(),
                    "{path} answered {status}"
                );
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "a handler is waiting on the relay before its no-device check"
            );
        }
    }
}
