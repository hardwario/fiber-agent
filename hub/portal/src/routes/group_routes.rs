//! Sensor-group CRUD and the monitoring page's per-operator view mode.
//!
//! The page itself renders from `http::dashboard_handler`; these are the
//! mutations behind its New Group / edit / delete dialogs, plus
//! `GET /export/group/{id}.csv`.
//!
//! Every mutation is a plain full-page POST followed by a redirect, not an
//! htmx fragment swap. Creating, renaming or deleting a group reshuffles the
//! whole page (a section appears, empties or gains rows), so a redirect is
//! both simpler and more correct than trying to patch it in place — the same
//! reasoning as `/beacons/add`.
//!
//! Membership arrives as a repeated `member[]` field, one per checked box,
//! encoded as `<kind>:<identity>`:
//!
//!     member[]=probe:0            wired line 0
//!     member[]=node:aabbcc…       Node by node_key
//!     member[]=beacon:AA:BB:…     Beacon by MAC (colons and all)
//!
//! Field selection rides along as a flat repeated `field[]`, one entry per
//! checked box, `<token>|<field>`:
//!
//!     field[]=node:aabbcc…|temperature
//!
//! A member with no `field[]` entry keeps the "all fields" default (the
//! `NULL` in the column). Splitting the token on the FIRST colon is what
//! makes the MAC case work, since a MAC is full of them; splitting the
//! field entry on `|` is why that character cannot appear in a field name,
//! which no registry field does.
//!
//! (This paragraph described a `fields[<kind>:<identity>][]` shape the code
//! has never implemented. Corrected with MB1, which is the first thing to
//! parse it on the server for anything but storage — see
//! [monitoring.backend.md §14](../../../plan/portal/monitoring.backend.md).)
//!
//! Since MB1 a save is **refused** when it would take a field another group
//! already holds: `monitoring::claims` builds the same map the picker grays
//! boxes out from, and `monitoring::conflicts` compares the submission
//! against it. The picker's disabled checkbox posts nothing, but a
//! hand-written POST is under no obligation to respect it.

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use axum_extra::extract::Form;
use portal_storage::{MoveDirection, NewSensorGroupMember, Storage};
use serde::Deserialize;

use crate::auth::{CurrentUser, RequireManager};
use crate::http::AppState;
use crate::monitoring;

use super::history_routes::{csv_field, format_rfc3339, today_yyyymmdd, CSV_HEADER};
use super::{audit_action, non_empty, render_fragment};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/monitoring/groups", post(create_group))
        .route("/monitoring/groups/{id}", post(update_group))
        .route("/monitoring/groups/{id}/delete", post(delete_group))
        .route("/monitoring/groups/{id}/move", post(move_group))
        // MB2's two live-row fragments. GET, so no `MUTATING_ROUTES` row —
        // and registered here, between the mutations and `/export`, so that
        // neither span the `every_mutating_route_declares_a_role` sweep cuts
        // (a registration runs to the *next* `.route(`) contains a `post(`
        // and hands one of them a phantom mutation.
        .route(
            "/monitoring/groups/{id}/rows/{token}",
            get(group_row_fragment),
        )
        .route("/monitoring/groups/{id}/header", get(group_header_fragment))
        // New group and edit group as pages (monitoring.md, dodatek
        // 2026-10-14); the POSTs above are unchanged.
        .route("/monitoring/groups/new", get(group_new_page))
        .route("/monitoring/groups/{id}/edit", get(group_edit_page))
        .route("/monitoring/view-mode", post(set_view_mode))
        .route("/export/group/{id_csv}", get(export_group))
}

// ---------------------------------------------------------------------
// Membership form
// ---------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GroupForm {
    name: String,
    description: String,
    /// The group's location (V40); blank clears it.
    location: String,
    #[serde(rename = "member[]")]
    members: Vec<String>,
    /// One `field[]` entry per selected field, `<kind>:<identity>|<field>`.
    /// A member with no entry here keeps the "all fields" default.
    #[serde(rename = "field[]")]
    fields: Vec<String>,
}

impl GroupForm {
    /// Turns the flat form into rows to insert. Unparseable entries are
    /// dropped rather than failing the save: the only way to produce one is
    /// to hand-craft the POST, and a partially-saved group the operator can
    /// see and fix beats a 400 they cannot.
    fn to_members(&self, hostname: &str) -> Vec<NewSensorGroupMember> {
        // The channel table (monitoring.md, dodatek 2026-10-15) posts a
        // Node's or Beacon's rows as `field[]` alone: the owner of a field
        // entry is a member even without its own `member[]`.
        let mut tokens: Vec<&str> = Vec::new();
        let owners = self
            .fields
            .iter()
            .filter_map(|f| f.split_once('|'))
            .map(|(o, _)| o);
        for token in self.members.iter().map(String::as_str).chain(owners) {
            if !tokens.contains(&token) {
                tokens.push(token);
            }
        }
        tokens
            .into_iter()
            .filter_map(|token| {
                let (kind, identity) = token.split_once(':')?;
                // HS-8 (Hub sweep 2026-09-26): a Node's field is a registry name, and anything
                // else made a phantom row; the identity is compared in its stored case below,
                // so `7c:…` could not slip past a group holding `7C:…`.
                let selected: Vec<String> = self
                    .fields
                    .iter()
                    .filter_map(|f| f.split_once('|'))
                    .filter(|(owner, _)| *owner == token)
                    .map(|(_, field)| field.trim().to_string())
                    .filter(|field| {
                        kind != "node" || fiber_protocol::lorawan_field(field).is_some()
                    })
                    .collect();
                // R-12 (review of !102): a member whose field tokens were all dropped is not
                // "all fields", which would claim the whole Node from every other group; it
                // is dropped too.
                let had_fields = self
                    .fields
                    .iter()
                    .filter_map(|f| f.split_once('|'))
                    .any(|(owner, _)| owner == token);
                if had_fields && selected.is_empty() {
                    return None;
                }
                let fields = (!selected.is_empty()).then_some(selected);

                let mut m = NewSensorGroupMember {
                    hostname: hostname.to_string(),
                    sensor_line: None,
                    node_key: None,
                    mac: None,
                    fields,
                };
                match kind {
                    "probe" => m.sensor_line = Some(identity.parse().ok()?),
                    "node" => m.node_key = Some(identity.trim().to_ascii_lowercase()),
                    "beacon" => m.mac = Some(identity.trim().to_ascii_uppercase()),
                    _ => return None,
                }
                Some(m)
            })
            .collect()
    }

    fn location(&self) -> Option<String> {
        let t = self.location.trim();
        (!t.is_empty()).then(|| t.chars().take(super::NAME_MAX).collect())
    }

    fn description(&self) -> Option<&str> {
        non_empty(self.description.clone()).map(|_| self.description.trim())
    }
}

/// The Hub every group member is stamped with.
///
/// **It is the *first* device in the store, and sensor groups are single-Hub
/// by [decisions.md](../../../plan/decisions.md) #8.** With two Hubs
/// reporting, every member of every group is recorded against whichever one
/// the map yields first, so multi-Hub grouping is already wrong here —
/// upstream of `monitoring::member_token`, which carries no hostname either
/// and would collide two Hubs' `probe:0`. The two are one constraint, not
/// two bugs: widening the token without fixing this would change nothing.
/// Multi-Hub grouping is phase-2 work with its own design; recorded in
/// [monitoring.backend.md §14](../../../plan/portal/monitoring.backend.md)
/// rather than worked around.
fn current_hostname(state: &AppState) -> Option<String> {
    state
        .store
        .snapshot()
        .devices
        .values()
        .next()
        .map(|d| d.hostname.clone())
}

/// Tells every *other* open tab that the group arrangement moved (MB2).
///
/// Only on a mutation that actually changed something: a refused save or a
/// no-op move reshuffles nothing, and a `refresh` for it would reload every
/// tab in the building for no reason. The acting session does not need this
/// at all — its POST redirects, which is a full page load already.
///
/// Coarse on purpose: a create or a delete adds or removes a whole
/// `<section>`, so there is no fragment to swap. See `ChangeEvent::Groups`.
fn announce_group_change(state: &AppState) {
    state.store.emit(crate::state::ChangeEvent::Groups);
}

/// Back to the page, with an error slug the monitoring template turns into a
/// translated banner.
/// `GET /monitoring/groups/new` and `/monitoring/groups/{id}/edit`: the
/// group form on its own page. Manager+, like the POSTs it submits to; a
/// viewer is sent back to `/`, as `/nodes/add` sends one to `/nodes`.
async fn group_form_page(current_user: CurrentUser, state: &AppState, id: Option<i64>) -> Response {
    if !crate::render::can_edit_thresholds(&current_user) {
        return Redirect::to("/").into_response();
    }
    let portal_state = state.store.snapshot();
    let groups = state
        .db
        .run(|s| s.list_sensor_groups())
        .await
        .unwrap_or_default();
    let members = state
        .db
        .run(|s| s.list_all_group_members())
        .await
        .unwrap_or_default();
    let liveness = state.offline.liveness_snapshot();
    let (title, action, g) = match id {
        None => (
            super::tr("monitoring.groups.new_title"),
            "/monitoring/groups".to_string(),
            None,
        ),
        Some(id) => {
            let Some(mut g) =
                monitoring::group_edit_ctxs(&portal_state, &groups, &members, &liveness)
                    .into_iter()
                    .find(|g| g.id == id)
            else {
                return super::not_found(state, Some(&current_user));
            };
            g.location = state
                .db
                .run(|s| s.list_sensor_group_locations())
                .await
                .unwrap_or_default()
                .into_iter()
                .find_map(|(gid, loc)| (gid == id).then_some(loc));
            (
                super::tr("monitoring.groups.edit_title"),
                format!("/monitoring/groups/{id}"),
                Some(g),
            )
        }
    };
    let picker = monitoring::picker(&portal_state, &groups, &members, &liveness, None);
    let rows = crate::sensor_picker::group_rows(
        &portal_state,
        g.as_ref().map_or(&picker, |g| &g.picker),
        g.as_ref().map_or(&[][..], |g| &g.selected),
        g.as_ref().map_or(&[][..], |g| &g.selected_fields),
    );
    // The Quantity filter's options: every quantity the table lists.
    let mut fields: Vec<(String, String)> = Vec::new();
    for r in rows.iter().filter(|r| !r.field.is_empty()) {
        if !fields.iter().any(|(n, _)| *n == r.field) {
            fields.push((r.field.clone(), r.quantity.clone()));
        }
    }
    let ctx = crate::render::page_ctx(
        minijinja::context! {
            nav_active => "dashboard",
            rows => rows,
            title => title,
            action => action,
            picker => picker,
            g => g,
            locations => super::location_suggestions(state).await,
            many_hubs => portal_state.devices.len() > 1,
            fields => fields,
        },
        Some(&current_user),
    );
    super::render(state, "pages/group_form.html", ctx)
}

async fn group_new_page(current_user: CurrentUser, State(state): State<AppState>) -> Response {
    group_form_page(current_user, &state, None).await
}

async fn group_edit_page(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    group_form_page(current_user, &state, Some(id)).await
}

/// Stores the group's location after the save (V40). A failure is logged,
/// not surfaced: the membership the operator came for is already saved.
async fn save_location(state: &AppState, id: i64, location: Option<String>) {
    if let Err(err) = state
        .db
        .write(move |s| s.set_sensor_group_location(id, location.as_deref()))
        .await
    {
        tracing::warn!(%err, id, "failed to store sensor group location");
    }
}

fn back(error: Option<&str>) -> Response {
    match error {
        Some(code) => Redirect::to(&format!("/?error={code}")).into_response(),
        None => Redirect::to("/").into_response(),
    }
}

// ---------------------------------------------------------------------
// Create / update / delete
// ---------------------------------------------------------------------

async fn create_group(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Form(form): Form<GroupForm>,
) -> Response {
    let Some(name) = non_empty(form.name.clone()) else {
        return back(Some("name_required"));
    };
    let Some(hostname) = current_hostname(&state) else {
        return back(Some("no_device"));
    };
    let members = form.to_members(&hostname);
    let description = form.description().map(str::to_string);

    let name = name.trim().to_string();

    let member_count = members.len();
    let result = ({
        let a0 = name.clone();
        state
            .db
            .write(move |s| s.create_sensor_group(&a0, description.as_deref(), &members))
    })
    .await;

    // The id is only known on success; a failed create has no group to name,
    // so the target degrades to the group that was asked for.
    let target = match &result {
        Ok(id) => format!("group:{id}"),
        Err(_) => "group:-".to_string(),
    };
    audit_action(
        &state,
        &current_user,
        "group_create",
        &target,
        serde_json::json!({ "name": name, "members": member_count }),
        result.is_ok(),
    )
    .await;

    match result {
        Ok(id) => {
            save_location(&state, id, form.location()).await;
            announce_group_change(&state);
            back(None)
        }
        Err(err) => {
            tracing::warn!(%err, "failed to create sensor group");
            back(Some("save_failed"))
        }
    }
}

async fn update_group(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<GroupForm>,
) -> Response {
    let Some(name) = non_empty(form.name.clone()) else {
        return back(Some("name_required"));
    };
    let Some(hostname) = current_hostname(&state) else {
        return back(Some("no_device"));
    };
    let members = form.to_members(&hostname);
    let description = form.description().map(str::to_string);

    let name = name.trim().to_string();

    let member_count = members.len();
    let result = ({
        let a1 = name.clone();
        state
            .db
            .write(move |s| s.update_sensor_group(id, &a1, description.as_deref(), &members))
    })
    .await;

    // `Ok(false)` is a real failure to audit, not a quiet nothing: it means
    // the group the operator edited no longer exists.
    audit_action(
        &state,
        &current_user,
        "group_update",
        &format!("group:{id}"),
        serde_json::json!({ "name": name, "members": member_count }),
        matches!(result, Ok(true)),
    )
    .await;

    match result {
        Ok(true) => {
            save_location(&state, id, form.location()).await;
            announce_group_change(&state);
            back(None)
        }
        Ok(false) => back(Some("not_found")),
        Err(err) => {
            tracing::warn!(%err, id, "failed to update sensor group");
            back(Some("save_failed"))
        }
    }
}

async fn delete_group(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    let result = state.db.write(move |s| s.delete_sensor_group(id)).await;
    audit_action(
        &state,
        &current_user,
        "group_delete",
        &format!("group:{id}"),
        serde_json::json!({ "deleted": matches!(result, Ok(true)) }),
        matches!(result, Ok(true)),
    )
    .await;

    match result {
        Ok(deleted) => {
            if deleted {
                announce_group_change(&state);
            }
            back(None)
        }
        Err(err) => {
            tracing::warn!(%err, id, "failed to delete sensor group");
            back(Some("save_failed"))
        }
    }
}

// ---------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MoveForm {
    direction: String,
}

/// One place up or down, like every other CRUD mutation here: a full-page
/// POST and a redirect. Reordering moves a whole section past another, so
/// there is no fragment worth swapping.
///
/// A no-op — an unknown id, `up` on the first group, `down` on the last — is
/// audited with `success: false` and comes back with the `move_failed`
/// banner. The arrows at the ends are not rendered, so the usual way to reach
/// one is a stale page or a hand-made POST (HS-17: this comment said "no
/// banner").
async fn move_group(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<MoveForm>,
) -> Response {
    let direction = match form.direction.as_str() {
        "up" => MoveDirection::Up,
        "down" => MoveDirection::Down,
        _ => return back(Some("move_failed")),
    };

    let result = state
        .db
        .write(move |s| s.move_sensor_group(id, direction))
        .await;
    let moved = matches!(result, Ok(true));
    audit_action(
        &state,
        &current_user,
        "group_move",
        &format!("group:{id}"),
        serde_json::json!({ "direction": form.direction, "moved": moved }),
        moved,
    )
    .await;

    match result {
        Ok(true) => {
            announce_group_change(&state);
            back(None)
        }
        Ok(false) => back(Some("move_failed")),
        Err(err) => {
            tracing::warn!(%err, id, "failed to move sensor group");
            back(Some("move_failed"))
        }
    }
}

// ---------------------------------------------------------------------
// View mode
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ViewModeForm {
    mode: String,
}

/// Any signed-in operator may set their own layout — it is a personal
/// preference on their own account, not a change anyone else sees.
async fn set_view_mode(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Form(form): Form<ViewModeForm>,
) -> Response {
    let Some(mode) = monitoring::canonical_view_mode(&form.mode) else {
        return back(Some("bad_view_mode"));
    };
    if let Err(err) = ({
        let a1 = mode.to_string();
        state
            .db
            .write(move |s| s.set_monitoring_view_mode(current_user.id, &a1))
    })
    .await
    {
        tracing::warn!(%err, user = %current_user.username, "failed to store monitoring view mode");
    }
    back(None)
}

// ---------------------------------------------------------------------
// Live-row fragments (MB2)
// ---------------------------------------------------------------------

/// A 200 with an empty body — what a fragment route answers when there is
/// nothing to show.
///
/// **Not a 404.** htmx does not swap on a non-2xx response, so a 404 would
/// strand the row on whatever it displayed before; 200-with-nothing makes it
/// disappear, which is the truthful rendering of "this group, member or
/// device is gone". Same rule and the same reason as
/// `GET /settings/users/{id}` (`user_routes.rs`).
///
/// Reserved for the four **genuine misses** monitoring.backend.md §15.5
/// names — unknown group, unparseable token, a token this group does not
/// hold, a device the store no longer has. A failed *read* is not one of
/// them: see [`read_failed`].
fn nothing() -> Response {
    axum::response::Html("").into_response()
}

/// Why a fragment has no markup to answer with — and therefore *which*
/// empty answer it owes the browser. The two are not interchangeable
/// (review round 2, monitoring.backend.md §15.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoFragment {
    /// One of §15.5's four genuine misses. [`nothing`]: 200-empty, so the
    /// `outerHTML` swap removes the row — the truthful rendering of "this
    /// group, member or device is gone".
    Miss,
    /// A database read **failed**. 500-empty, so htmx declines the swap and
    /// the row keeps the markup it already has — stale for one nudge, and
    /// repainted by the next. Answering `Miss` here would take the row (or
    /// the whole section header) off the screen because SQLite was briefly
    /// unhappy, which is not a truthful rendering of "the query errored".
    ///
    /// The body is empty because nothing renders it: htmx discards a
    /// non-2xx response's body, and the operator's channel for a read
    /// failure is the row that did not change plus [`read_failed`]'s log
    /// line.
    ReadFailed,
}

impl IntoResponse for NoFragment {
    fn into_response(self) -> Response {
        match self {
            Self::Miss => nothing(),
            Self::ReadFailed => (StatusCode::INTERNAL_SERVER_ERROR, "").into_response(),
        }
    }
}

/// Maps one fragment read's outcome onto [`NoFragment::ReadFailed`], so that
/// the decision — an `Err` is 500, never 200 — lives in one testable place
/// instead of being repeated at each of the four read sites (three of which
/// got it wrong, two by way of `unwrap_or_default()`).
fn read_failed<T, E: std::fmt::Display>(
    result: Result<T, E>,
    id: i64,
    what: &str,
) -> Result<T, NoFragment> {
    result.map_err(|err| {
        tracing::warn!(%err, id, what, "a monitoring fragment read failed; row left as it was");
        NoFragment::ReadFailed
    })
}

/// The Beacon thresholds one row needs, keyed the way
/// [`monitoring::sections`] expects them.
///
/// The page loads the whole Hub's map once; a fragment for a single Node or
/// probe needs none of it, and a Beacon row needs exactly one MAC's — so
/// this reads one row's worth instead of the fleet's.
///
/// A read failure is propagated, not degraded to "no thresholds": empty
/// thresholds are indistinguishable from thresholds nobody has breached, so
/// swallowing the error would repaint the row green — a fault axis reporting
/// the *absence of a database read* as "in range" (review round 2).
async fn row_beacon_thresholds(
    state: &AppState,
    members: &[portal_storage::SensorGroupMember],
    id: i64,
) -> Result<monitoring::BeaconThresholds, NoFragment> {
    let mut out = monitoring::BeaconThresholds::new();
    for m in members {
        let Some(mac) = m.mac.as_deref() else {
            continue;
        };
        let thresholds = read_failed(
            ({
                let a0 = m.hostname.clone();
                let a1 = portal_storage::THRESHOLD_SCOPE_EYE.to_string();
                let a2 = mac.to_string();
                state
                    .db
                    .run(move |s| s.list_field_thresholds(&a0, &a1, &a2))
            })
            .await,
            id,
            "beacon thresholds",
        )?;
        out.insert(mac.to_string(), thresholds);
    }
    Ok(out)
}

/// `GET /monitoring/groups/{id}/rows/{token}` — one monitoring row, in the
/// **caller's** view mode.
///
/// The counterpart of the `mon-{kind}-{host}-{identity}` SSE nudge: the
/// frame carries an id and no markup, so every browser fetches its own row
/// here and gets it rendered in whichever of the four layouts that operator
/// chose. `partials/group_row.html` is the same partial the page loops over,
/// so the two cannot drift.
///
/// Gated with `CurrentUser` and nothing more: it renders exactly what the
/// `/monitoring` page already shows this session, and every read on that
/// page is viewer-visible.
async fn group_row_fragment(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path((id, token)): Path<(i64, String)>,
) -> Response {
    // `Ok` is the row, `Err` says which empty answer to send instead — a
    // genuine miss or a read failure. Split out so the body can `?` on both
    // without either collapsing into the other.
    row_fragment_inner(current_user, state, id, token)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

async fn row_fragment_inner(
    current_user: CurrentUser,
    state: AppState,
    id: i64,
    token: String,
) -> Result<Response, NoFragment> {
    let member_ref = monitoring::parse_token(&token).ok_or(NoFragment::Miss)?;
    let members = read_failed(
        state.db.run(move |s| s.list_group_members(id)).await,
        id,
        "members",
    )?;
    let member = members
        .iter()
        .find(|m| member_ref.matches(m))
        .ok_or(NoFragment::Miss)?;
    let groups = read_failed(state.db.run(|s| s.list_sensor_groups()).await, id, "groups")?;
    let group = groups.iter().find(|g| g.id == id).ok_or(NoFragment::Miss)?;

    let portal_state = state.store.snapshot();
    let device = portal_state
        .device(&member.hostname)
        .ok_or(NoFragment::Miss)?;
    let beacon_thresholds = row_beacon_thresholds(&state, std::slice::from_ref(member), id).await?;
    let mut row = monitoring::resolve_member(
        group,
        device,
        member,
        &beacon_thresholds,
        &state.offline.liveness_snapshot(),
    )
    .ok_or(NoFragment::Miss)?;
    monitoring::Descriptions::load(&state.db, &portal_state)
        .await
        .add_to_row(&mut row);

    let view_mode = monitoring::effective_view_mode(&current_user);
    // The detailed card and table print the 24 h min/max, so only they pay
    // for the history read behind it — the same rule the page applies.
    if view_mode == "card-detailed" || view_mode == "table-detailed" {
        monitoring::fill_row_sparkline(&mut row, &state.db, crate::render::now_unix()).await;
    }

    let ctx = crate::render::page_ctx(
        minijinja::context! {
            r => row,
            can_edit => crate::render::can_edit_thresholds(&current_user),
            view_cards => view_mode.starts_with("card"),
            view_simple => view_mode.ends_with("simplified"),
        },
        Some(&current_user),
    );
    Ok(render_fragment(&state, "partials/group_row.html", ctx))
}

/// `GET /monitoring/groups/{id}/header` — one group's section header.
///
/// Refetched on any of its rows' nudges, because every number in it is an
/// aggregate over the rows: a row that repainted alone would leave
/// "Mimo meze: 0" standing above a warning row, and §3.2 makes that
/// load-bearing — a collapsed group shows nothing *but* the header.
///
/// `is_first`/`is_last` come from `list_sensor_groups`' order, which is the
/// order the page renders, because a fragment has no `loop` to read them
/// off.
async fn group_header_fragment(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    header_fragment_inner(current_user, state, id)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

async fn header_fragment_inner(
    current_user: CurrentUser,
    state: AppState,
    id: i64,
) -> Result<Response, NoFragment> {
    let groups = read_failed(state.db.run(|s| s.list_sensor_groups()).await, id, "groups")?;
    let position = groups
        .iter()
        .position(|g| g.id == id)
        .ok_or(NoFragment::Miss)?;
    let members = read_failed(
        state.db.run(move |s| s.list_group_members(id)).await,
        id,
        "members",
    )?;

    let portal_state = state.store.snapshot();
    let beacon_thresholds = row_beacon_thresholds(&state, &members, id).await?;
    let mut sections = monitoring::sections(
        &portal_state,
        &groups[position..=position],
        &members,
        &beacon_thresholds,
        &state.offline.liveness_snapshot(),
    );
    monitoring::Descriptions::load(&state.db, &portal_state)
        .await
        .add_to_sections(&mut sections);
    let section = sections.into_iter().next().ok_or(NoFragment::Miss)?;

    let ctx = crate::render::page_ctx(
        minijinja::context! {
            s => section,
            can_edit => crate::render::can_edit_thresholds(&current_user),
            is_first => position == 0,
            is_last => position + 1 == groups.len(),
        },
        Some(&current_user),
    );
    Ok(render_fragment(&state, "partials/group_header.html", ctx))
}

// ---------------------------------------------------------------------
// CSV export
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ExportQuery {
    from: Option<String>,
    to: Option<String>,
}

/// One CSV for the whole group over a time range, rather than v1's dialog
/// that fires one download per member. Same data, one file: a group is a
/// unit an operator reports on, and a folder of eight CSVs is not that.
///
/// Column shape is `/export/sensor/{line}.csv`'s verbatim, so the two
/// exports can be opened in the same spreadsheet without reconciling
/// headers. Probe rows fill the temperature/min/max columns, Node and
/// Beacon rows fill `field`/`value` and leave those blank.
async fn export_group(
    current_user: CurrentUser,
    State(state): State<AppState>,
    Path(id_csv): Path<String>,
    Query(q): Query<ExportQuery>,
) -> Response {
    // Same trick as `/export/sensor/{line}.csv`: matchit cannot mix a literal
    // suffix into a `{param}` segment, so the route captures the whole
    // segment and this strips `.csv`.
    let Some(id) = id_csv
        .strip_suffix(".csv")
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };

    let (Some(from_raw), Some(to_raw)) = (q.from.as_deref(), q.to.as_deref()) else {
        return (StatusCode::BAD_REQUEST, "'from' and 'to' are required").into_response();
    };
    let (Some(from_ts), Some(to_ts)) = (
        crate::display_tz::parse_local(from_raw),
        crate::display_tz::parse_local(to_raw),
    ) else {
        return (StatusCode::BAD_REQUEST, "invalid date").into_response();
    };
    if to_ts <= from_ts {
        return (StatusCode::BAD_REQUEST, "'to' must be after 'from'").into_response();
    }
    if to_ts - from_ts > super::MAX_EXPORT_SPAN_S {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "export window is too wide; narrow it to 366 days or less",
        )
            .into_response();
    }

    // The whole file is read in ONE `db.run`: one trip to the blocking pool,
    // one hold of the connection's lock (#114). Until 2026-10-10 it took a
    // separate `db.run` for the group, the member list and every probe,
    // every Beacon and every *field* of every Node — dozens of acquisitions
    // for one group, each queued behind whatever the operator's own live
    // session (SSE renders, row and header fragments, `/stats/monitoring`)
    // was reading on the same connection at that moment. Now the live reads
    // wait for the export once instead of the export waiting on them dozens
    // of times. The rows, their order and the audit are unchanged
    // (`a_group_export_writes_every_member_kind_in_member_order`).
    let built = state
        .db
        .run(move |s| {
            let Some(group) = s.get_sensor_group(id)? else {
                return Ok(None);
            };
            let members = s.list_group_members(id).unwrap_or_default();

            let mut csv = String::from(CSV_HEADER);
            let mut ok = true;
            let mut row_count = 0usize;
            for m in &members {
                // `query_minutes` and friends are `[from, to)`; `+ 1` restores the
                // inclusive upper bound v1's SQL had, as in `export_csv`.
                let (rows, member_ok) = member_csv_rows(s, m, from_ts, to_ts + 1);
                row_count += rows.lines().count();
                csv.push_str(&rows);
                ok &= member_ok;
            }
            Ok(Some((group.name, csv, row_count, ok)))
        })
        .await;
    let (group_name, csv, row_count, ok) = match built {
        Ok(Some(built)) => built,
        Ok(None) => return (StatusCode::NOT_FOUND, "unknown group").into_response(),
        Err(err) => {
            tracing::error!(%err, id, "group export: failed to load the group");
            return (StatusCode::INTERNAL_SERVER_ERROR, "query failed").into_response();
        }
    };

    // The event type stays `"export"` — the same string the hand-rolled call
    // this replaced used — so audit filters saved against v1's trail keep
    // matching. What changes is that `details` is now populated and `success`
    // is the query's real outcome rather than a literal `true`.
    audit_action(
        &state,
        &current_user,
        "export",
        &format!("group:{id}"),
        // Unix seconds, as the history export writes them (S37-12: one filter for both).
        serde_json::json!({ "rows": row_count, "from": from_ts, "to": to_ts }),
        ok,
    )
    .await;

    let filename = format!("group_{}_{}.csv", slug(&group_name), today_yyyymmdd());
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        csv,
    )
        .into_response()
}

/// One member's rows. A query failure logs and contributes nothing rather
/// than failing the whole download — a partial export the operator can see
/// beats a 500 that says nothing about which member broke.
///
/// Synchronous over a connection the caller already holds, i.e. called from
/// inside a `db.run` closure: the export takes the lock once for the whole
/// file rather than once per query (#114).
pub(crate) fn member_csv_rows(
    s: &Storage,
    m: &portal_storage::SensorGroupMember,
    from_ts: i64,
    to_ts: i64,
) -> (String, bool) {
    let host = m.hostname.as_str();
    let mut out = String::new();
    // A failed member query still yields a file, minus that member's rows —
    // but the caller audits it as a failed export, because a CSV that is
    // silently short is the one thing an export must not be.
    let mut ok = true;

    if let Some(line) = m.sensor_line {
        match s.query_minutes(host, line, from_ts, to_ts) {
            Ok(rows) => {
                for r in &rows {
                    let avg = r.avg_c.map(|v| v.to_string()).unwrap_or_default();
                    let min = r.min_c.map(|v| v.to_string()).unwrap_or_default();
                    let max = r.max_c.map(|v| v.to_string()).unwrap_or_default();
                    out.push_str(&csv_row(
                        r.minute_ts,
                        host,
                        &line.to_string(),
                        "",
                        "temperature",
                        &avg,
                        &avg,
                        &min,
                        &max,
                    ));
                }
            }
            Err(err) => {
                ok = false;
                tracing::warn!(%err, host, line, "group export: probe query failed");
            }
        }
        return (out, ok);
    }

    if let Some(node_key) = &m.node_key {
        // A member with no field selection exports every field it has stored
        // history for; `fields` narrows that to what the group shows.
        let fields: Vec<String> = m.fields.clone().unwrap_or_else(|| {
            fiber_protocol::lorawan_registry()
                .iter()
                .filter(|f| f.kind == fiber_protocol::LorawanFieldKind::Continuous && f.chartable)
                .map(|f| f.name.to_string())
                .collect()
        });
        for field in fields {
            match s.query_field_readings(host, node_key, &field, from_ts, to_ts) {
                Ok(rows) => {
                    // Wire units (pressure in Pa, not the displayed hPa):
                    // a CSV is machine data (UX-038).
                    for r in &rows {
                        let value = r.value.map(|v| v.to_string()).unwrap_or_default();
                        out.push_str(&csv_row(
                            r.ts, host, "", node_key, &field, &value, "", "", "",
                        ));
                    }
                }
                Err(err) => {
                    ok = false;
                    tracing::warn!(%err, host, node_key, field, "group export: node query failed");
                }
            }
        }
        return (out, ok);
    }

    let Some(mac) = &m.mac else {
        return (out, ok);
    };
    match s.query_eye_readings(host, mac, from_ts, to_ts, None) {
        Ok(rows) => {
            for r in &rows {
                if let Some(t) = r.temperature_c {
                    out.push_str(&csv_row(
                        r.ts,
                        host,
                        "",
                        mac,
                        "temperature",
                        &t.to_string(),
                        "",
                        "",
                        "",
                    ));
                }
                if let Some(h) = r.humidity_pct {
                    out.push_str(&csv_row(
                        r.ts,
                        host,
                        "",
                        mac,
                        "humidity",
                        &h.to_string(),
                        "",
                        "",
                        "",
                    ));
                }
            }
        }
        Err(err) => {
            ok = false;
            tracing::warn!(%err, host, mac, "group export: beacon query failed");
        }
    }
    (out, ok)
}

#[allow(clippy::too_many_arguments)]
fn csv_row(
    ts: i64,
    device: &str,
    sensor_line: &str,
    entity: &str,
    field: &str,
    value: &str,
    temperature: &str,
    min_c: &str,
    max_c: &str,
) -> String {
    format!(
        "{},{},{},{},{},{},{},{},{}\r\n",
        csv_field(&format_rfc3339(ts)),
        csv_field(device),
        sensor_line,
        csv_field(entity),
        csv_field(field),
        csv_field(value),
        csv_field(temperature),
        csv_field(min_c),
        csv_field(max_c),
    )
}

/// Filename-safe group or probe name: Czech and Slovak diacritics folded to
/// their ASCII base letter, every other non-alphanumeric run collapsed to a
/// single `-`.
///
/// Still deliberately lossy — the `Content-Disposition` header must not
/// carry a quote or a newline, and the real name is inside the file's rows
/// anyway — but no longer *silently* lossy: before 2026-09-10 every accented
/// letter became a dash of its own, so "Chladicí box 3" filed itself as
/// `chladic--box-3` while monitoring.md §9 decision 12 promised an ASCII
/// transliteration. The table is the two languages this portal ships
/// (i18n/{cs,en}); anything outside it still degrades to `-`, which is the
/// honest answer for a script this function cannot romanise.
pub(crate) fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            continue;
        }
        match deaccent(c) {
            Some(ascii) => out.push(ascii),
            // Collapsed here rather than in a second pass: a run of
            // punctuation, spaces and unromanisable letters is one
            // separator, not one per character.
            None => {
                if !out.ends_with('-') {
                    out.push('-');
                }
            }
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "group".to_string()
    } else {
        trimmed
    }
}

/// One accented Latin letter to its lowercase ASCII base, or `None` for a
/// character that is not one.
///
/// Czech (áčďéěíňóřšťúůýž) and Slovak (äĺľŕôö) plus the German ü/ö/ä that
/// turn up in device labels — written out rather than derived, because
/// stripping combining marks needs Unicode normalisation tables this binary
/// does not carry and would still answer nothing useful for ů.
fn deaccent(c: char) -> Option<char> {
    let lower = c.to_lowercase().next()?;
    Some(match lower {
        'á' | 'à' | 'â' | 'ä' | 'ă' | 'ą' | 'å' => 'a',
        'ç' | 'č' | 'ć' => 'c',
        'ď' | 'đ' => 'd',
        'é' | 'è' | 'ê' | 'ë' | 'ě' | 'ę' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ĺ' | 'ľ' | 'ł' => 'l',
        'ň' | 'ń' | 'ñ' => 'n',
        'ó' | 'ò' | 'ô' | 'ö' | 'ő' | 'ø' => 'o',
        'ŕ' | 'ř' => 'r',
        'š' | 'ś' | 'ş' => 's',
        'ť' | 'ţ' => 't',
        'ú' | 'ù' | 'û' | 'ü' | 'ů' | 'ű' => 'u',
        'ý' | 'ÿ' => 'y',
        'ž' | 'ź' | 'ż' => 'z',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HS-8: identities in their stored case, and a Node field only when the registry knows it.
    #[test]
    fn members_are_normalised_before_the_exclusivity_check() {
        let form = GroupForm {
            name: "g".into(),
            description: String::new(),
            location: String::new(),
            members: vec![
                "node:5876070000000ABC".into(),
                "beacon:7c:d9:f4:00:00:01".into(),
            ],
            fields: vec![
                "node:5876070000000ABC|Temperature".into(),
                "node:5876070000000ABC|humidity".into(),
            ],
        };
        let members = form.to_members("h1");
        assert_eq!(members[0].node_key.as_deref(), Some("5876070000000abc"));
        assert_eq!(members[0].fields, Some(vec!["humidity".to_string()]));
        assert_eq!(members[1].mac.as_deref(), Some("7C:D9:F4:00:00:01"));
    }

    /// R-12: a member whose only field was unknown is dropped, not widened to all fields.
    #[test]
    fn a_member_left_without_fields_is_dropped() {
        let form = GroupForm {
            name: "g".into(),
            description: String::new(),
            location: String::new(),
            members: vec!["node:5876070000000abc".into()],
            fields: vec!["node:5876070000000abc|bogus".into()],
        };
        assert!(form.to_members("h1").is_empty());
    }

    /// The channel table posts a Node's channels as `field[]` alone; the
    /// owner becomes a member with exactly those fields (dodatek 2026-10-15).
    #[test]
    fn a_field_entry_alone_makes_its_owner_a_member() {
        let f = form(
            &["probe:1"],
            &[
                "node:aabb|temperature",
                "node:aabb|humidity",
                "beacon:AA:01|temperature",
            ],
        );
        let m = f.to_members("hub");
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].sensor_line, Some(1));
        assert_eq!(m[1].node_key.as_deref(), Some("aabb"));
        assert_eq!(
            m[1].fields,
            Some(vec!["temperature".to_string(), "humidity".to_string()])
        );
        assert_eq!(m[2].mac.as_deref(), Some("AA:01"));
    }

    #[test]
    fn a_blank_location_clears_and_a_long_one_is_capped() {
        let mut f = form(&[], &[]);
        f.location = "   ".into();
        assert_eq!(f.location(), None);
        f.location = format!(" {} ", "x".repeat(100));
        assert_eq!(f.location().map(|l| l.len()), Some(super::super::NAME_MAX));
    }

    fn form(members: &[&str], fields: &[&str]) -> GroupForm {
        GroupForm {
            name: "G".into(),
            description: String::new(),
            location: String::new(),
            members: members.iter().map(|s| s.to_string()).collect(),
            fields: fields.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn each_kind_lands_in_its_own_identity_column() {
        let f = form(
            &[
                "probe:3",
                "node:aabbccdd00000001",
                "beacon:AA:BB:CC:DD:EE:01",
            ],
            &[],
        );
        let m = f.to_members("hub");
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].sensor_line, Some(3));
        assert_eq!(m[1].node_key.as_deref(), Some("aabbccdd00000001"));
        // Split on the FIRST colon only — a MAC is full of them.
        assert_eq!(m[2].mac.as_deref(), Some("AA:BB:CC:DD:EE:01"));
    }

    #[test]
    fn field_selection_attaches_to_its_own_member_only() {
        let f = form(
            &["node:aaa", "node:bbb"],
            &["node:aaa|temperature", "node:aaa|humidity"],
        );
        let m = f.to_members("hub");
        assert_eq!(
            m[0].fields.as_ref().expect("aaa has a selection"),
            &vec!["temperature".to_string(), "humidity".to_string()]
        );
        assert_eq!(m[1].fields, None, "bbb keeps the all-fields default");
    }

    #[test]
    fn junk_members_are_dropped_not_fatal() {
        let f = form(&["probe:not-a-number", "nonsense", "probe:1"], &[]);
        let m = f.to_members("hub");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].sensor_line, Some(1));
    }

    #[test]
    fn slug_survives_a_name_that_would_break_the_header() {
        assert_eq!(slug("Cold \"Storage\"\n"), "cold-storage");
        assert_eq!(slug("///"), "group");
        assert_eq!(slug(""), "group");
    }

    /// monitoring.md §9 decision 12 promises the PNG (and CSV) filename is
    /// an **ASCII transliteration** of the name on screen. Until 2026-09-10
    /// it was not: every accented letter became a dash of its own, so a
    /// Czech name arrived as `chladic--box-3` (MB3 review item 16).
    #[test]
    fn a_czech_name_transliterates_instead_of_falling_apart() {
        assert_eq!(slug("Chladicí box 3"), "chladici-box-3");
        assert_eq!(slug("Mrazák — sklad Č"), "mrazak-sklad-c");
        assert_eq!(slug("ŽLUŤOUČKÝ kůň"), "zlutoucky-kun");
        // Runs collapse, and the ends are trimmed.
        assert_eq!(slug("  a   b  "), "a-b");
        // A script this table cannot romanise still degrades to one dash,
        // not one per character.
        assert_eq!(slug("温度 box"), "box");
        assert_eq!(slug("温度"), "group");
    }

    /// Review round 2's defect, held at the one place that decides it: a
    /// failed **read** and a genuine **miss** must not answer the same way.
    ///
    /// The fragments swap `outerHTML`, so a 200 with an empty body deletes
    /// the row (or the whole section header) from the page. That is the right
    /// answer for the four misses monitoring.backend.md §15.5 names, and the
    /// wrong one for a query that errored — where the row must stay put and
    /// htmx must decline the swap, which only a non-2xx does. Before this
    /// commit three of the four reads laundered `Err` into
    /// `unwrap_or_default()`/`nothing()` and a transient SQLite failure took
    /// a group off the screen.
    ///
    /// A unit test on the mapping rather than an end-to-end one: nothing in
    /// this workspace can make an open `Db` connection fail on demand
    /// (`Storage` exposes no raw SQL and holds its file open, so neither
    /// dropping the tempdir nor a key mismatch reaches a *query*), and the
    /// four read sites all go through this helper.
    #[test]
    fn a_failed_fragment_read_is_five_hundred_while_a_genuine_miss_stays_two_hundred() {
        let failed: Result<Vec<i64>, portal_storage::StorageError> = Err(
            portal_storage::StorageError::Io(std::io::Error::other("disk is having a day")),
        );
        assert_eq!(
            read_failed(failed, 7, "groups").expect_err("an Err classifies"),
            NoFragment::ReadFailed
        );
        assert_eq!(
            NoFragment::ReadFailed.into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "htmx declines the swap, so the row keeps the markup it has"
        );
        assert_eq!(
            NoFragment::Miss.into_response().status(),
            StatusCode::OK,
            "a genuine miss is still 200-empty, so the row disappears"
        );

        let ok: Result<i64, portal_storage::StorageError> = Ok(42);
        assert_eq!(read_failed(ok, 7, "groups").ok(), Some(42));
    }

    /// MB0's *Accept:* — "every group mutation leaves an `audit_log` row
    /// carrying the real outcome, proven by oneshot tests". The four
    /// mutations above wrote no row at all before this stage.
    mod integration {
        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use fiber_protocol::DeviceStatus;
        use tower::ServiceExt;

        use crate::auth::{Role, SESSION_COOKIE};
        use crate::http::AppState;
        use crate::routes::test_support::{login_as_role, test_state};
        use crate::state::events::IngestEvent;

        /// `current_hostname` reads the store, so a group cannot be created
        /// until some Hub exists.
        async fn state_with_a_hub() -> (AppState, tempfile::TempDir) {
            let (state, dir) = test_state().await;
            state.store.apply(IngestEvent::Status {
                hostname: "h1".into(),
                status: DeviceStatus::Online,
            });
            (state, dir)
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

        /// Every audit row of one type, newest first.
        async fn audit_rows(state: &AppState, event_type: &str) -> Vec<portal_storage::AuditEntry> {
            state
                .auth
                .audit_page(portal_storage::AuditFilter {
                    event_type: Some(event_type.into()),
                    username: None,
                    since: None,
                    until: None,
                    success: None,
                    search: None,
                    limit: 50,
                    offset: 0,
                    ..Default::default()
                })
                .await
                .expect("audit page")
                .0
        }

        /// New and edit are pages (monitoring.md, dodatek 2026-10-14): a
        /// manager gets the form, a viewer goes back to `/`, an unknown group
        /// is a 404.
        #[tokio::test]
        async fn the_group_form_is_a_page_for_a_manager() {
            let (state, _dir) = state_with_a_hub().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            let router = super::super::router().with_state(state.clone());
            post(
                &router,
                "/monitoring/groups",
                &manager,
                "name=Seed&location=Sklad+A",
            )
            .await;
            let id = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list")[0]
                .id;
            let get = |path: String, token: String| {
                let router = router.clone();
                async move {
                    router
                        .oneshot(
                            Request::builder()
                                .uri(path)
                                .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                                .body(Body::empty())
                                .unwrap(),
                        )
                        .await
                        .expect("response")
                }
            };
            let new = get("/monitoring/groups/new".into(), manager.clone()).await;
            assert_eq!(new.status(), StatusCode::OK);
            let edit = get(format!("/monitoring/groups/{id}/edit"), manager.clone()).await;
            assert_eq!(edit.status(), StatusCode::OK);
            let body = axum::body::to_bytes(edit.into_body(), usize::MAX)
                .await
                .expect("body");
            let html = String::from_utf8_lossy(&body).replace("&#x2f;", "/");
            assert!(html.contains(&format!(r#"action="/monitoring/groups/{id}""#)));
            assert!(html.contains(r#"value="Seed""#), "prefilled");
            // The shared metadata fields and the channel table (2026-10-15).
            assert!(html.contains(r#"name="location""#), "the location combobox");
            assert!(html.contains("data-select-count"), "the channel table");
            assert!(html.contains("Sklad A"), "the stored location, prefilled");
            // Embedded inline text edit fields (ui_components.md §1): the
            // edit look only, no view mode, saved by the form's own Save.
            assert!(
                html.contains(r#"id="group-name" data-te-in required"#),
                "{html}"
            );
            assert!(!html.contains("data-te-view"), "no view mode: {html}");
            let missing = get("/monitoring/groups/9999/edit".into(), manager).await;
            assert_eq!(missing.status(), StatusCode::NOT_FOUND);
            let refused = get("/monitoring/groups/new".into(), viewer).await;
            assert_eq!(refused.status(), StatusCode::SEE_OTHER);
        }

        /// A group takes any number of channels of one Node (amendment
        /// 2026-10-15): four of them save, and the Monitoring card shows all
        /// four.
        #[tokio::test]
        async fn a_member_carries_any_number_of_fields() {
            let (state, _dir) = state_with_a_hub().await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(crate::state::events::NodeReadingMsg {
                    message_id: "m-aa11".into(),
                    dev_eui: "aa11".into(),
                    serial: Some("SN-aa11".into()),
                    fields: [
                        ("temperature".to_string(), 4.0),
                        ("humidity".to_string(), 50.0),
                        ("pressure".to_string(), 1000.0),
                        ("illuminance".to_string(), 300.0),
                    ]
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
                    received_at: "2026-09-03T12:00:00Z".to_string(),
                }),
            });
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());

            let ok = post(
                &router,
                "/monitoring/groups",
                &manager,
                "name=Machine&member%5B%5D=node%3Aaa11\
                 &field%5B%5D=node%3Aaa11%7Ctemperature\
                 &field%5B%5D=node%3Aaa11%7Chumidity\
                 &field%5B%5D=node%3Aaa11%7Cpressure\
                 &field%5B%5D=node%3Aaa11%7Cilluminance",
            )
            .await;
            assert_eq!(ok.status(), StatusCode::SEE_OTHER);
            let groups = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list");
            assert_eq!(groups.len(), 1);
            let members = state
                .db
                .run(|s| s.list_all_group_members())
                .await
                .expect("members");
            assert_eq!(members.len(), 1);
            assert_eq!(
                members[0].fields.as_deref().map(<[String]>::len),
                Some(4),
                "all four channels stored"
            );

            let sections = crate::monitoring::sections(
                &state.store.snapshot(),
                &groups,
                &members,
                &Default::default(),
                &Default::default(),
            );
            let row = &sections[0].rows[0];
            assert_eq!(row.fields.len(), 4, "the card shows every channel");
        }

        #[tokio::test]
        async fn a_managers_group_crud_is_audited_with_its_real_outcome() {
            let (state, _dir) = state_with_a_hub().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());

            let created = post(
                &router,
                "/monitoring/groups",
                &manager,
                "name=Cold+Storage&member%5B%5D=probe%3A3",
            )
            .await;
            assert_eq!(created.status(), StatusCode::SEE_OTHER);
            let rows = audit_rows(&state, "group_create").await;
            assert_eq!(rows.len(), 1);
            assert!(rows[0].success);
            let details = rows[0].details.as_deref().expect("details");
            assert!(
                details.contains("Cold Storage") && details.contains("\"members\":1"),
                "the row says what was created: {details}"
            );

            let id = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list")[0]
                .id;

            let updated = post(
                &router,
                &format!("/monitoring/groups/{id}"),
                &manager,
                "name=Freezers",
            )
            .await;
            assert_eq!(updated.status(), StatusCode::SEE_OTHER);
            assert!(audit_rows(&state, "group_update").await[0].success);

            // A group that is not there is the outcome an audit trail is read
            // for, so it is a row with `success = false`, not silence.
            post(&router, "/monitoring/groups/9999", &manager, "name=Ghosts").await;
            let updates = audit_rows(&state, "group_update").await;
            assert_eq!(updates.len(), 2);
            assert!(
                updates.iter().any(|r| !r.success),
                "updating a missing group is audited as a failure"
            );

            let deleted = post(
                &router,
                &format!("/monitoring/groups/{id}/delete"),
                &manager,
                "",
            )
            .await;
            assert_eq!(deleted.status(), StatusCode::SEE_OTHER);
            assert!(audit_rows(&state, "group_delete").await[0].success);
        }

        #[tokio::test]
        async fn a_move_is_audited_with_its_direction_and_whether_it_moved() {
            let (state, _dir) = state_with_a_hub().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());

            for name in ["a", "b"] {
                post(
                    &router,
                    "/monitoring/groups",
                    &manager,
                    &format!("name={name}"),
                )
                .await;
            }
            let ids: Vec<i64> = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list")
                .into_iter()
                .map(|g| g.id)
                .collect();

            let moved = post(
                &router,
                &format!("/monitoring/groups/{}/move", ids[1]),
                &manager,
                "direction=up",
            )
            .await;
            assert_eq!(moved.status(), StatusCode::SEE_OTHER);
            let names: Vec<String> = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list")
                .into_iter()
                .map(|g| g.name)
                .collect();
            assert_eq!(names, ["b", "a"], "the order is the page's, and it moved");

            let rows = audit_rows(&state, "group_move").await;
            assert_eq!(rows.len(), 1);
            assert!(rows[0].success);
            let details = rows[0].details.as_deref().expect("details");
            assert!(details.contains("\"direction\":\"up\""), "{details}");

            // `up` on what is now the first group: no arrow renders for it,
            // so reaching this needs a hand-made POST — audited as a refusal.
            post(
                &router,
                &format!("/monitoring/groups/{}/move", ids[1]),
                &manager,
                "direction=up",
            )
            .await;
            let rows = audit_rows(&state, "group_move").await;
            assert_eq!(rows.len(), 2);
            assert!(
                rows.iter().any(|r| !r.success),
                "a no-op move is audited as one"
            );
        }

        /// The gate is an extractor, so a viewer's POST never reaches the
        /// handler body — which is why the refusal leaves no audit row.
        #[tokio::test]
        async fn a_viewer_is_refused_every_group_mutation_and_leaves_no_trail() {
            let (state, _dir) = state_with_a_hub().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());
            post(&router, "/monitoring/groups", &manager, "name=Seed").await;
            let id = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list")[0]
                .id;

            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            for (path, body) in [
                ("/monitoring/groups".to_string(), "name=Nope"),
                (format!("/monitoring/groups/{id}"), "name=Nope"),
                (format!("/monitoring/groups/{id}/delete"), ""),
                (format!("/monitoring/groups/{id}/move"), "direction=up"),
            ] {
                let response = post(&router, &path, &viewer, body).await;
                assert_eq!(response.status(), StatusCode::FORBIDDEN, "POST {path}");
            }

            for event in ["group_create", "group_update", "group_delete", "group_move"] {
                let rows = audit_rows(&state, event).await;
                assert!(
                    rows.iter()
                        .all(|r| r.username.as_deref() != Some("viewer1")),
                    "a 403 leaves no {event} row"
                );
            }
            assert_eq!(
                state
                    .db
                    .run(|s| s.list_sensor_groups())
                    .await
                    .expect("list")
                    .len(),
                1,
                "and nothing was written"
            );
        }

        // ---- MB1: per-field exclusivity (§4 WP1.1) --------------------

        /// Seeds one Hub reporting a Node with two fields, so the picker and
        /// the exclusivity check have something real to split.
        async fn state_with_a_two_field_node() -> (AppState, tempfile::TempDir) {
            let (state, dir) = state_with_a_hub().await;
            state.store.apply(IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "n1".into(),
                payload: Box::new(crate::state::events::NodeReadingMsg {
                    message_id: "m-n1".into(),
                    dev_eui: "n1".into(),
                    serial: Some("SN-n1".into()),
                    fields: [
                        ("temperature".to_string(), 4.0),
                        ("humidity".to_string(), 50.0),
                    ]
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
                    received_at: "2026-09-03T12:00:00Z".to_string(),
                }),
            });
            (state, dir)
        }

        /// §4's *Accept:* — "a Node's temperature in group A and humidity in
        /// group B round-trips, and each group's card shows only its own
        /// field". The split is the whole point of MB1.
        #[tokio::test]
        async fn a_nodes_two_fields_split_across_two_groups() {
            let (state, _dir) = state_with_a_two_field_node().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());

            for (name, field) in [("A", "temperature"), ("B", "humidity")] {
                let response = post(
                    &router,
                    "/monitoring/groups",
                    &manager,
                    &format!("name={name}&member%5B%5D=node%3An1&field%5B%5D=node%3An1%7C{field}"),
                )
                .await;
                assert_eq!(response.status(), StatusCode::SEE_OTHER, "group {name}");
            }

            let groups = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list");
            assert_eq!(groups.len(), 2, "both saved");
            let members = state
                .db
                .run(|s| s.list_all_group_members())
                .await
                .expect("members");
            assert_eq!(members.len(), 2);

            let sections = crate::monitoring::sections(
                &state.store.snapshot(),
                &groups,
                &members,
                &Default::default(),
                &Default::default(),
            );
            let held = |name: &str| -> Vec<String> {
                let s = sections.iter().find(|s| s.name == name).expect("section");
                s.rows[0]
                    .fields
                    .iter()
                    .filter_map(|f| f.field.clone())
                    .collect()
            };
            assert_eq!(held("A"), ["temperature"]);
            assert_eq!(held("B"), ["humidity"]);
            // One card each, and the two rows are addressable separately.
            assert_ne!(sections[0].rows[0].row_id, sections[1].rows[0].row_id);
            assert_eq!(sections[0].field_count, 1);
        }

        /// A group mixes quantities freely: one Node's temperature and humidity
        /// go on one card with both values, nothing restricts a group to one
        /// quantity.
        #[tokio::test]
        async fn a_group_mixes_temperature_and_humidity() {
            let (state, _dir) = state_with_a_two_field_node().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());

            let response = post(
                &router,
                "/monitoring/groups",
                &manager,
                "name=Mixed&member%5B%5D=node%3An1\
                 &field%5B%5D=node%3An1%7Ctemperature\
                 &field%5B%5D=node%3An1%7Chumidity",
            )
            .await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            assert_ne!(
                response
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok()),
                Some("/?error=too_many_fields")
            );
            let groups = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list");
            let members = state
                .db
                .run(|s| s.list_all_group_members())
                .await
                .expect("members");
            assert_eq!(groups.len(), 1);
            let sections = crate::monitoring::sections(
                &state.store.snapshot(),
                &groups,
                &members,
                &Default::default(),
                &Default::default(),
            );
            let row = &sections[0].rows[0];
            let fields: Vec<String> = row.fields.iter().filter_map(|f| f.field.clone()).collect();
            assert_eq!(fields, ["temperature", "humidity"]);
            assert_eq!(sections[0].field_count, 2);
        }

        /// Exclusivity is gone (monitoring.md, amended 2026-10-15): a field
        /// one group holds can join another, whole-entity or single.
        #[tokio::test]
        async fn two_groups_may_share_a_field() {
            let (state, _dir) = state_with_a_two_field_node().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());

            for body in [
                "name=A&member%5B%5D=node%3An1&field%5B%5D=node%3An1%7Ctemperature",
                "name=B&member%5B%5D=node%3An1&field%5B%5D=node%3An1%7Ctemperature",
                "name=C&member%5B%5D=node%3An1",
            ] {
                let res = post(&router, "/monitoring/groups", &manager, body).await;
                assert_eq!(
                    res.headers()
                        .get(header::LOCATION)
                        .and_then(|v| v.to_str().ok()),
                    Some("/"),
                    "{body}"
                );
            }
            assert_eq!(
                state
                    .db
                    .run(|s| s.list_sensor_groups())
                    .await
                    .expect("list")
                    .len(),
                3
            );
        }

        /// `update_sensor_group` replaces membership wholesale, so a group
        /// re-submitting what it already holds must not collide with itself.
        #[tokio::test]
        async fn a_group_may_resubmit_its_own_fields_on_update() {
            let (state, _dir) = state_with_a_two_field_node().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());

            post(
                &router,
                "/monitoring/groups",
                &manager,
                "name=A&member%5B%5D=node%3An1&field%5B%5D=node%3An1%7Ctemperature",
            )
            .await;
            let id = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list")[0]
                .id;

            let renamed = post(
                &router,
                &format!("/monitoring/groups/{id}"),
                &manager,
                "name=A2&member%5B%5D=node%3An1&field%5B%5D=node%3An1%7Ctemperature",
            )
            .await;
            assert_eq!(
                renamed
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/"),
                "no error slug"
            );
            assert_eq!(
                state
                    .db
                    .run(|s| s.list_sensor_groups())
                    .await
                    .expect("list")[0]
                    .name,
                "A2"
            );
            assert!(audit_rows(&state, "group_update").await[0].success);
        }

        /// The export keeps the `"export"` event type it had before MB0 — so
        /// filters saved against the old trail keep matching — but gains the
        /// `details` its hand-rolled call passed as `None`.
        #[tokio::test]
        async fn an_export_is_audited_with_its_range_and_row_count() {
            let (state, _dir) = state_with_a_hub().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());
            post(
                &router,
                "/monitoring/groups",
                &manager,
                "name=Cold&member%5B%5D=probe%3A0",
            )
            .await;
            let id = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list")[0]
                .id;

            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(format!(
                            "/export/group/{id}.csv?from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z"
                        ))
                        .header(header::COOKIE, format!("{SESSION_COOKIE}={manager}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK);

            let rows = audit_rows(&state, "export").await;
            assert_eq!(rows.len(), 1, "the event type is unchanged");
            assert!(rows[0].success);
            let details = rows[0].details.as_deref().expect("details");
            assert!(
                details.contains("\"rows\":0") && details.contains("\"from\":1767225600"),
                "{details}"
            );
        }

        /// The whole file, byte for byte, for a group holding every member
        /// kind: a probe, a Node with a field selection, a Node without one
        /// (every chartable Continuous field) and a Beacon. Members come out
        /// in membership order, a Node's fields in selection or registry
        /// order, each series by time; the upper bound is inclusive; fields
        /// the member does not export (unselected, not chartable) and rows
        /// outside the range are absent.
        ///
        /// Pinned before #114 moved every member read under one lock, so a
        /// change in how the export reads the database cannot change what
        /// it writes.
        #[tokio::test]
        async fn a_group_export_writes_every_member_kind_in_member_order() {
            use portal_storage::{EyeReading, MinuteReading, NodeFieldReading, SOURCE_LIVE};

            let (state, _dir) = state_with_a_hub().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());
            let created = post(
                &router,
                "/monitoring/groups",
                &manager,
                "name=Cold+room&member%5B%5D=probe%3A0\
                 &member%5B%5D=node%3Aaa11\
                 &field%5B%5D=node%3Aaa11%7Chumidity\
                 &field%5B%5D=node%3Aaa11%7Ctemperature\
                 &member%5B%5D=node%3Abb22\
                 &member%5B%5D=beacon%3AAA%3ABB%3ACC%3ADD%3AEE%3A01",
            )
            .await;
            assert_eq!(
                created
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/"),
                "the group was created"
            );
            let id = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list")[0]
                .id;

            // 2026-01-01T00:00:00Z .. 2026-01-02T00:00:00Z
            let (from, to) = (1_767_225_600_i64, 1_767_312_000_i64);
            {
                let s = state.db.lock().expect("db");
                let minute = |minute_ts, avg_c, min_c, max_c| MinuteReading {
                    hostname: "h1".into(),
                    sensor_line: 0,
                    minute_ts,
                    min_c,
                    avg_c,
                    max_c,
                    sample_count: Some(60),
                    disconnect_count: Some(0),
                    worst_alarm: None,
                    source: SOURCE_LIVE.into(),
                    data_hmac: None,
                };
                for row in [
                    minute(from - 60, Some(9.0), Some(9.0), Some(9.0)),
                    minute(from, Some(4.5), Some(4.0), Some(5.0)),
                    minute(from + 3600, Some(-1.25), None, Some(0.5)),
                    minute(to + 60, Some(9.0), Some(9.0), Some(9.0)),
                ] {
                    s.upsert_minute(&row).expect("seed minute");
                }
                let reading = |node_key: &str, field: &str, ts, value| NodeFieldReading {
                    hostname: "h1".into(),
                    node_key: node_key.into(),
                    field: field.into(),
                    ts,
                    value,
                    alarm_state: None,
                    source: SOURCE_LIVE.into(),
                };
                for row in [
                    reading("aa11", "temperature", from + 400, Some(21.5)),
                    reading("aa11", "humidity", from + 400, Some(45.0)),
                    reading("aa11", "humidity", to, Some(46.5)),
                    reading("aa11", "pressure", from + 400, Some(1013.0)),
                    reading("bb22", "pressure", from + 1400, Some(1013.2)),
                    reading("bb22", "voltage", from + 1400, Some(3.1)),
                    reading("bb22", "temperature", from + 2400, Some(19.0)),
                    reading("bb22", "temperature", from + 2460, None),
                ] {
                    s.insert_field_reading(&row).expect("seed field reading");
                }
                let eye = |ts, temperature_c, humidity_pct| EyeReading {
                    hostname: "h1".into(),
                    mac: "AA:BB:CC:DD:EE:01".into(),
                    ts,
                    temperature_c,
                    humidity_pct,
                    event_type: "live".into(),
                    message_id: None,
                };
                for row in [
                    eye(from + 900, Some(3.5), Some(80)),
                    eye(from + 930, Some(3.25), None),
                    eye(from + 960, None, Some(79)),
                    eye(to + 1, Some(9.0), Some(9)),
                ] {
                    s.insert_eye_reading(&row).expect("seed eye reading");
                }
            }

            let response = get(
                &router,
                &format!(
                    "/export/group/{id}.csv?from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z"
                ),
                &manager,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE).unwrap(),
                "text/csv; charset=utf-8"
            );
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_DISPOSITION)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                format!(
                    "attachment; filename=\"group_cold-room_{}.csv\"",
                    super::super::today_yyyymmdd()
                )
            );
            assert_eq!(
                body_of(response).await,
                "timestamp,device,sensor_line,dev_eui,field,value,temperature,min_celsius,max_celsius\r\n\
                 2026-01-01T00:00:00Z,h1,0,,temperature,4.5,4.5,4,5\r\n\
                 2026-01-01T01:00:00Z,h1,0,,temperature,-1.25,-1.25,,0.5\r\n\
                 2026-01-01T00:06:40Z,h1,,aa11,humidity,45,,,\r\n\
                 2026-01-02T00:00:00Z,h1,,aa11,humidity,46.5,,,\r\n\
                 2026-01-01T00:06:40Z,h1,,aa11,temperature,21.5,,,\r\n\
                 2026-01-01T00:40:00Z,h1,,bb22,temperature,19,,,\r\n\
                 2026-01-01T00:41:00Z,h1,,bb22,temperature,,,,\r\n\
                 2026-01-01T00:23:20Z,h1,,bb22,pressure,1013.2,,,\r\n\
                 2026-01-01T00:15:00Z,h1,,AA:BB:CC:DD:EE:01,temperature,3.5,,,\r\n\
                 2026-01-01T00:15:00Z,h1,,AA:BB:CC:DD:EE:01,humidity,80,,,\r\n\
                 2026-01-01T00:15:30Z,h1,,AA:BB:CC:DD:EE:01,temperature,3.25,,,\r\n\
                 2026-01-01T00:16:00Z,h1,,AA:BB:CC:DD:EE:01,humidity,79,,,\r\n"
            );

            let rows = audit_rows(&state, "export").await;
            assert_eq!(rows.len(), 1);
            assert!(rows[0].success);
            let details = rows[0].details.as_deref().expect("details");
            assert!(details.contains("\"rows\":12"), "{details}");
        }

        // ---- MB2: the two live-row fragments (§5) --------------------

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

        async fn body_of(response: axum::response::Response) -> String {
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .expect("body");
            String::from_utf8(bytes.to_vec()).expect("utf8")
        }

        async fn set_view_mode(state: &AppState, token: &str, mode: &str) {
            let user = state.auth.validate(token).await.expect("session");
            ({
                let a1 = mode.to_string();
                state
                    .db
                    .write(move |s| s.set_monitoring_view_mode(user.id, &a1))
            })
            .await
            .expect("store view mode");
        }

        /// The whole reason the nudge carries no markup: **one route, two
        /// sessions, two shapes.** A broadcast fragment would have to pick
        /// one of the four layouts for everybody; here each browser asks for
        /// its own and the same partial answers differently.
        #[tokio::test]
        async fn two_view_modes_get_their_own_row_shape_from_the_same_route() {
            let (state, _dir) = state_with_a_two_field_node().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());
            post(
                &router,
                "/monitoring/groups",
                &manager,
                "name=Cold&member%5B%5D=node%3An1",
            )
            .await;
            let id = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list")[0]
                .id;

            let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
            set_view_mode(&state, &manager, "table-detailed").await;
            set_view_mode(&state, &viewer, "card-detailed").await;

            let path = format!("/monitoring/groups/{id}/rows/node:n1");
            let table = get(&router, &path, &manager).await;
            assert_eq!(table.status(), StatusCode::OK);
            let table = body_of(table).await;
            let cards = get(&router, &path, &viewer).await;
            // A viewer may read it — it is the page they already see.
            assert_eq!(cards.status(), StatusCode::OK);
            let cards = body_of(cards).await;

            assert!(table.trim_start().starts_with("<tbody "), "{table}");
            assert!(
                !cards.contains("<tr "),
                "the card layout has no row: {cards}"
            );
            assert!(cards.contains("card-v1"), "{cards}");
            // Both are the same row and both listen on the same nudge.
            for html in [&table, &cards] {
                assert!(
                    html.contains(&format!("id=\"row-{id}-node-h1-n1\"")),
                    "{html}"
                );
                assert!(html.contains("hx-trigger=\"sse:mon-node-h1-n1\""), "{html}");
                assert!(
                    html.contains(&format!("hx-get=\"/monitoring/groups/{id}/rows/node:n1\"")),
                    "{html}"
                );
            }
        }

        /// A miss is 200 with an empty body, never a 404: htmx does not swap
        /// on a non-2xx, so a 404 would strand the row on stale markup
        /// instead of letting it disappear.
        #[tokio::test]
        async fn a_row_fragment_that_resolves_to_nothing_is_two_hundred_and_empty() {
            let (state, _dir) = state_with_a_two_field_node().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());
            post(
                &router,
                "/monitoring/groups",
                &manager,
                "name=Cold&member%5B%5D=node%3An1",
            )
            .await;
            let id = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list")[0]
                .id;

            for path in [
                // a token nothing can parse
                format!("/monitoring/groups/{id}/rows/nonsense"),
                // a well-formed token this group does not hold
                format!("/monitoring/groups/{id}/rows/node:ghost"),
                // a member of a group that does not exist
                "/monitoring/groups/9999/rows/node:n1".to_string(),
                // a header for a group that does not exist
                "/monitoring/groups/9999/header".to_string(),
            ] {
                let response = get(&router, &path, &manager).await;
                assert_eq!(response.status(), StatusCode::OK, "GET {path}");
                assert!(body_of(response).await.is_empty(), "GET {path}");
            }
        }

        /// The header is refetched because its numbers are aggregates over
        /// the rows — a row that repainted alone would leave "Out of range: 0"
        /// standing above a warning row.
        #[tokio::test]
        async fn the_header_fragment_carries_the_groups_own_counts() {
            let (state, _dir) = state_with_a_two_field_node().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());
            post(
                &router,
                "/monitoring/groups",
                &manager,
                "name=Cold&member%5B%5D=node%3An1",
            )
            .await;
            let id = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list")[0]
                .id;

            let html = body_of(
                get(
                    &router,
                    &format!("/monitoring/groups/{id}/header"),
                    &manager,
                )
                .await,
            )
            .await;
            assert!(html.contains(">2 channels<"), "the field count: {html}");
            assert!(
                !html.contains("Out of range"),
                "the badge is hidden at zero: {html}"
            );
            assert!(
                !html.contains("Faults:"),
                "and so is the fault badge: {html}"
            );
            // Only one group, so both arrows are inert — computed from the
            // stored order, since a fragment has no `loop` to read. Matched
            // with the trailing newline, or `disabled:opacity-30` on the same
            // buttons counts twice more.
            assert_eq!(html.matches("disabled\n").count(), 2, "{html}");
            assert!(
                html.contains("hx-trigger=\"sse:mon-node-h1-n1\""),
                "the header listens on its rows' nudges: {html}"
            );
        }

        /// F8: an empty group listens on nothing, but htmx defaults a
        /// non-form element carrying `hx-get` to `click` — so an absent
        /// `hx-trigger` turns the whole header into a link that silently
        /// replaces itself. The trigger has to be written out and inert.
        #[tokio::test]
        async fn an_empty_groups_header_carries_an_inert_trigger_not_a_click() {
            let (state, _dir) = state_with_a_hub().await;
            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());
            post(&router, "/monitoring/groups", &manager, "name=Empty").await;
            let id = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list")[0]
                .id;

            let html = body_of(
                get(
                    &router,
                    &format!("/monitoring/groups/{id}/header"),
                    &manager,
                )
                .await,
            )
            .await;
            assert!(html.contains("hx-get="), "{html}");
            assert!(
                html.contains("hx-trigger=\"sse:mon-none\""),
                "an explicit inert trigger, never an absent one: {html}"
            );
        }

        /// The WIP re-keyed `fill_sparklines` off the member token and
        /// quietly fixed a live bug — MB1's group-id prefix had broken the
        /// Beacon case, because the `trim_start_matches` it used to parse the
        /// DOM id fails silently when it no longer matches. Nothing held it
        /// until now.
        #[tokio::test]
        async fn a_beacon_rows_sparkline_is_resolved_from_its_token() {
            let (state, dir) = state_with_a_hub().await;
            let mac = "AA:BB:CC:DD:EE:01";
            state.store.apply(IngestEvent::EyeSensors {
                hostname: "h1".into(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: false,
                    tags: vec![fiber_protocol::payloads::eye::EyeTag {
                        gateway: None,
                        mac: mac.into(),
                        name: Some("Freezer".into()),
                        temperature_c: Some(-18.0),
                        humidity_pct: Some(40),
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
                        alarm_state: Some("NORMAL".into()),
                        ble_config: None,
                    }],
                },
            });

            let storage = portal_storage::Storage::open(&dir.path().join("portal.db"), None)
                .expect("open storage");
            let now = crate::render::now_unix();
            for (n, t) in [(-18.0), (-17.5), (-18.5)].iter().enumerate() {
                storage
                    .insert_eye_reading(&portal_storage::EyeReading {
                        hostname: "h1".into(),
                        mac: mac.into(),
                        ts: now - 600 + n as i64,
                        temperature_c: Some(*t),
                        humidity_pct: Some(40),
                        event_type: "live".into(),
                        message_id: Some(format!("m{n}")),
                    })
                    .expect("seed reading");
            }

            let manager = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());
            post(
                &router,
                "/monitoring/groups",
                &manager,
                "name=Cold&member%5B%5D=beacon%3AAA%3ABB%3ACC%3ADD%3AEE%3A01\
                 &field%5B%5D=beacon%3AAA%3ABB%3ACC%3ADD%3AEE%3A01%7Ctemperature",
            )
            .await;
            let id = state
                .db
                .run(|s| s.list_sensor_groups())
                .await
                .expect("list")[0]
                .id;
            set_view_mode(&state, &manager, "card-detailed").await;

            // The MAC's colons ride the path segment unescaped — they are
            // valid `pchar`, and the token splits on the first one.
            let html = body_of(
                get(
                    &router,
                    &format!("/monitoring/groups/{id}/rows/beacon:{mac}"),
                    &manager,
                )
                .await,
            )
            .await;
            assert!(html.contains("Freezer"), "the row resolved: {html}");
            assert!(
                html.contains("<polyline"),
                "and its trend line was filled from the token: {html}"
            );
        }
    }
}
