//! HTTP route handlers grouped by area: session lifecycle (`auth_routes`),
//! admin user management (`user_routes`), the audit log (`audit_routes`),
//! the history/chart/export surface (`history_routes`), the Hub family —
//! pairing, system commands and wired-line thresholds (`hub_routes`),
//! Nodes/Beacons
//! (`node_routes`/`beacon_routes`), and the M6 alarm + notification-settings
//! surface (`alarm_routes`/`notification_routes`), and the unauthenticated
//! CA download a joining Client Hub fetches (`connection_routes`). Each module owns its
//! request/response shapes and merges its own sub-`Router` into
//! `http::app`'s top-level router; this file only holds what's shared
//! across them.

pub mod alarm_routes;
pub mod audit_routes;
pub mod auth_routes;
pub mod beacon_routes;
pub mod calibration_routes;
pub(crate) mod channels;
pub mod cluster_routes;
pub(crate) mod command;
pub mod connection_routes;
pub mod docs_routes;
pub mod group_routes;
pub mod history_routes;
pub mod hub_routes;
pub mod lang_routes;
pub mod limits_routes;
pub mod node_routes;
pub mod notification_routes;
pub mod probe_routes;
pub mod settings_routes;
pub mod user_routes;

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use minijinja::context;
use minijinja::value::Value;

use crate::auth::CurrentUser;
use crate::http::AppState;
use crate::i18n;
use crate::render;

/// The `?error=` code of the dashboard and the probe detail as its message, or `None` for a code
/// the catalogue does not know (HS-9, Hub sweep 2026-09-26): an arbitrary code rendered the raw
/// key and wrote a warning per request.
pub(crate) fn groups_error(code: &str) -> Option<String> {
    let key = format!("monitoring.groups.err_{code}");
    (crate::i18n::lookup("en", &key) != key).then(|| tr(&key))
}

/// Looks up `key` in the current request's language. The i18n mechanism
/// (`i18n.rs`) is meant to be driven from templates via the `t` filter; this
/// is the same lookup for the handful of messages a route builds in Rust
/// (page-level flash messages, synthesized `CommandOutcome`s) before they
/// ever reach a template.
pub(crate) fn tr(key: &str) -> String {
    crate::i18n::lookup(crate::i18n::current_lang(), key)
}

/// One Hub of `partials/unpaired_banner.html` (GitLab #109): its hostname, and whether it is the
/// Hub this portal runs on — the one that pairs without a typed code.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct UnpairedHub {
    pub hostname: String,
    pub local: bool,
}

/// The `unpaired` context of `partials/unpaired_banner.html`: every page whose actions need a
/// signature carries the same banner, and an admin pairs right there instead of being sent to
/// the Hub's System tab and back (devices.hub.md §3, dodatek 2026-10-13).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct UnpairedBanner {
    /// The Hubs this portal is not paired with, in page order; empty renders nothing.
    pub hubs: Vec<UnpairedHub>,
    /// Admin: the pairing form inline — the gate `POST /hubs/{hub}/pair` has.
    pub can_pair: bool,
    /// Where a successful pairing returns to: the page the banner is on.
    pub return_to: String,
}

impl UnpairedBanner {
    /// No Hub among `hostnames` can sign anything: a registration form has nothing to submit to.
    pub(crate) fn blocks_all(&self, hostnames: &[String]) -> bool {
        !hostnames.is_empty() && self.hubs.len() == hostnames.len()
    }
}

/// The Hubs among `hostnames` this portal is not paired with, in the order given, as the banner
/// of a page at `return_to`. Every role sees it, as on the Hub's own tabs (which build theirs
/// in the template): it is a state, not a permission.
pub(crate) async fn unpaired_banner(
    state: &AppState,
    current_user: &CurrentUser,
    hostnames: &[String],
    return_to: &str,
) -> UnpairedBanner {
    let mut hubs = Vec::new();
    for hostname in hostnames {
        if !state.keys.is_paired(hostname).await {
            hubs.push(UnpairedHub {
                hostname: hostname.clone(),
                local: hub_routes::is_local_device(hostname, &state.local_hostname),
            });
        }
    }
    UnpairedBanner {
        hubs,
        can_pair: current_user.is_admin(),
        return_to: return_to.to_string(),
    }
}

/// `?hub=<hostname>` for the routes that are Hub-scoped but sit outside
/// `/hubs/{hub}`: the wired-line chart and history, the bulk threshold
/// panel, and the two registration POSTs. The rule the parameter feeds is
/// [`crate::state::PortalState::scoped_hub`] — an explicit hostname, else
/// the only Hub, else a 404 (devices.hub.md §12 E).
#[derive(serde::Deserialize)]
pub(crate) struct HubQuery {
    pub(crate) hub: Option<String>,
}

/// Sets `status` on a rendered page, unless the render itself failed: a broken template stays
/// the 500 it is instead of passing for a 404 or a 429 (R2-6, review 2 of !102).
pub(crate) fn with_status(mut response: Response, status: StatusCode) -> Response {
    if response.status().is_success() {
        *response.status_mut() = status;
    }
    response
}

/// The 404 page inside the layout, as the router fallback renders it.
/// The tabs of a Node's and a Beacon's detail page, addressed as path
/// segments like the Hub's (`/nodes/{key}/{tab}`, amended 2026-10-14).
pub(crate) const DEVICE_TABS: [&str; 4] = ["overview", "settings", "history", "diagnostics"];

/// `/nodes/{key}` and `/beacons/{mac}` land on a tab. The old `?tab=x`
/// form (bookmarks, alarm links) is moved into the path; an unknown or
/// missing tab lands on Overview. Every other query pair is kept as is.
pub(crate) fn device_tab_redirect(base: &str, raw: Option<&str>) -> Response {
    let mut tab = "overview";
    let mut rest = Vec::new();
    for pair in raw.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        match pair.strip_prefix("tab=") {
            Some(v) => {
                if let Some(t) = DEVICE_TABS.iter().find(|t| **t == v) {
                    tab = t;
                }
            }
            None => rest.push(pair),
        }
    }
    let mut to = format!("{base}/{tab}");
    if !rest.is_empty() {
        to.push('?');
        to.push_str(&rest.join("&"));
    }
    axum::response::Redirect::to(&to).into_response()
}

pub(crate) fn not_found(state: &AppState, current_user: Option<&CurrentUser>) -> Response {
    let body = render(
        state,
        "pages/not_found.html",
        render::page_ctx(minijinja::context! {}, current_user),
    );
    (axum::http::StatusCode::NOT_FOUND, body).into_response()
}

/// [`not_found`] saying what is missing: `message` replaces the generic
/// line under the 404 (`pages/not_found.html`'s `message` override).
pub(crate) fn not_found_saying(
    state: &AppState,
    current_user: Option<&CurrentUser>,
    message: String,
) -> Response {
    let body = render(
        state,
        "pages/not_found.html",
        render::page_ctx(minijinja::context! { message => message }, current_user),
    );
    (axum::http::StatusCode::NOT_FOUND, body).into_response()
}

/// Renders `template` with `ctx`. Mirrors `http.rs`'s dashboard handler:
/// degrades to the M0 placeholder when templates aren't installed at all,
/// and logs + 500s on a render error rather than ever panicking.
///
/// M9: this is the page/fragment render path's single merge point for
/// `lang` (`i18n.rs`'s module doc) — every handler across every route
/// module funnels through here, so `lang` reaches every one of them without
/// any of their call sites changing.
///
/// P1 adds `stats` for the same reason: `layout/base.html` renders the
/// connection pill and the four probe metrics above every authenticated
/// page (v1 does it from its `(dashboard)` layout), and no page should have
/// to carry that in its own context. **A fragment does not go through here**
/// — it has no layout to draw them in, so it would pay for a state snapshot
/// and a fleet-wide count nothing reads; that is [`render_fragment`].
///
/// Also threads `experimental_features_enabled` (`AppState`, from
/// `PortalConfig`) the same way — `layout/nav.html`'s `"experimental"` gate
/// reads it to hide non-core nav entries, and any future non-core page can
/// check it directly, without every call site plumbing it through
/// `render::page_ctx` by hand.
pub(crate) fn render(state: &AppState, template: &str, ctx: Value) -> Response {
    if !state.templates_available {
        return "portal — templates not installed".into_response();
    }
    // The sidebar pill, drawn server-side from the count the last
    // `/alarms/badge` fetch left cached. The element still fetches itself on
    // `load`, so the number stays true; what it no longer does is paint an
    // empty box on every navigation and pop the pill in a moment later.
    let ctx = context! {
        nav_alarm_count => state.alarms.cached_active_count(),
        ..ctx
    };
    // A page that built its own row keeps it — `/alarms` counts rows in the
    // event log, which takes a database read this synchronous function
    // cannot do. Every other page gets the row its `nav_active` implies.
    let ctx = if ctx.get_attr("stats").is_ok_and(|v| !v.is_undefined()) {
        ctx
    } else {
        let scope = render::StatsScope::for_nav(
            ctx.get_attr("nav_active")
                .ok()
                .as_ref()
                .and_then(|v| v.as_str()),
        );
        context! {
            stats => render::page_stats_ctx(
                &state.store.snapshot(),
                &state.offline.liveness_snapshot(),
                state.alarms.hub_faults(),
                scope,
            ),
            ..ctx
        }
    };
    render_fragment(state, template, ctx)
}

/// [`render`] for a **fragment**: everything but `stats`.
///
/// `stats` is the four tiles `layout/base.html` draws above every
/// authenticated *page*. A fragment has no layout, so it never reads them —
/// and building them is not free: one full `PortalState` clone plus one
/// verdict-table clone, then a pass over every entity of every device. MB2
/// made that per-swap rather than per-page (two fragment requests per
/// reading per holding group, monitoring.backend.md §15.6), which is what
/// review round 2 caught.
///
/// `lang` and `experimental_features_enabled` stay: the first is what every
/// `|t` in a partial reads, and the second is cheap and lets a partial gate
/// on it as a page can. If a fragment ever does need the tiles, it wants
/// [`render`] — with the snapshot it already holds, not a second one.
pub(crate) fn render_fragment(state: &AppState, template: &str, ctx: Value) -> Response {
    if !state.templates_available {
        return "portal — templates not installed".into_response();
    }
    let ctx = context! {
        lang => i18n::current_lang(),
        experimental_features_enabled => state.experimental_features_enabled,
        ..ctx
    };
    match state
        .templates
        .env()
        .get_template(template)
        .and_then(|tmpl| tmpl.render(ctx))
    {
        Ok(html) => Html(html).into_response(),
        Err(err) => {
            // minijinja nests the real failure (bad filter argument, undefined
            // attribute) under a generic "could not render include" — without
            // the chain a broken partial only says which file, not what.
            let mut detail = err.to_string();
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                detail.push_str(&format!(" <- {cause}"));
                source = cause.source();
            }
            tracing::warn!(%detail, template, "failed to render template");
            (StatusCode::INTERNAL_SERVER_ERROR, "template render failed").into_response()
        }
    }
}

/// The description cap of the metadata card (metadata.cs.md): counted live
/// in the browser, enforced again here.
pub(crate) const DESCRIPTION_MAX: usize = 255;

/// The name cap of the metadata card, as the input's `maxlength`.
pub(crate) const NAME_MAX: usize = 64;

/// The location suggestions: the DISTINCT set of locations every Hub,
/// probe, Node and Beacon carries right now. Derived on each call, never
/// stored. Case-insensitive on trimmed text, first spelling wins, sorted.
pub(crate) async fn location_suggestions(state: &AppState) -> Vec<String> {
    let snap = state.store.snapshot();
    let mut raw: Vec<String> = Vec::new();
    for dev in snap.devices.values() {
        raw.extend(dev.sensors.iter().filter_map(|s| s.location.clone()));
        raw.extend(dev.nodes.values().filter_map(|n| n.location.clone()));
        raw.extend(dev.ble_sensors.values().filter_map(|b| b.location.clone()));
    }
    // Group locations (V40) suggest too: a group is often named after a room.
    if let Ok(rows) = state.db.run(|s| s.list_sensor_group_locations()).await {
        raw.extend(rows.into_iter().map(|(_, l)| l));
    }
    match state.db.run(|s| s.distinct_meta_locations()).await {
        Ok(rows) => raw.extend(rows),
        Err(err) => tracing::warn!(%err, "failed to read metadata locations"),
    }
    dedup_locations(raw)
}

pub(crate) fn dedup_locations(raw: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for l in raw {
        let t = l.trim();
        if !t.is_empty() && seen.insert(t.to_lowercase()) {
            out.push(t.to_string());
        }
    }
    out.sort_by_key(|a| a.to_lowercase());
    out
}

/// A blank/whitespace-only optional form field (e.g. "full name" left
/// empty) becomes `None` rather than `Some("")`.
pub(crate) fn non_empty(s: String) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Writes one `audit_log` row for a privileged device action
/// (devices.backend.md §3, WP0.1: "every privileged action ... writes an
/// `audit_log` row through the existing insert path").
///
/// Thin over [`crate::auth::AuthService::audit`], which is fire-and-forget by
/// design — an audit failure is logged, never propagated, so it cannot block
/// the action it describes. Two things it fixes at every call site:
///
/// - `success` is the action's real outcome, not `true`. A refused or
///   timed-out command is exactly what an audit trail is read for, and the
///   pre-WP0.1 call sites all passed a literal `true`.
/// - `details` is a JSON object, matching what `audit_log.details` already
///   holds elsewhere, so the audit page renders one shape.
pub(crate) async fn audit_action(
    state: &AppState,
    actor: &CurrentUser,
    event_type: &str,
    target: &str,
    details: serde_json::Value,
    success: bool,
) {
    state
        .auth
        .audit(
            event_type,
            Some(actor),
            None,
            Some(target),
            serde_json::to_string(&details).ok(),
            success,
        )
        .await;
}

/// Parses an optional non-negative integer out of a form field: blank means
/// "inherit", not zero.
///
/// The distinction is the whole reason `device_meta`'s override columns are
/// nullable (V9's header): `offline_k_override` NULL takes the YAML default,
/// while an explicit `0` **disables** detection for that one device. A form
/// that coerced blank to 0 would silently turn every save into "stop
/// watching this device". Mirrors `node_routes::parse_optional_bound`, which
/// makes the same promise for threshold bounds.
/// Widest window a CSV export will answer, in seconds (366 days).
///
/// Both exports materialise every row into a `Vec` and then a second time
/// into an in-memory `String` before a byte is written, and the statements
/// underneath (`query_minutes`, `query_field_readings`) carry no `LIMIT` —
/// unlike their siblings `query_eye_readings` (clamped to 50 000) and
/// `list_raw_messages` (50). A viewer asking for 1970→2099 on a Node
/// reporting four fields a minute is ~2 M rows held twice; a handful of
/// those concurrently is the process. A year plus a day is past any real
/// export and still bounded.
pub(crate) const MAX_EXPORT_SPAN_S: i64 = 366 * 86_400;

/// Upper bound for [`parse_optional_count`]. Without one the column is an
/// unbounded `i64`, and a value the liveness arithmetic multiplies can wrap in
/// release (no `overflow-checks`) and panic in debug. A million is past
/// anything the field means: `offline_k` is a silence multiplier that is
/// single digits in practice.
pub(crate) const MAX_OPTIONAL_COUNT: i64 = 1_000_000;

/// Percent-encode one query-string VALUE.
///
/// Hand-rolled rather than pulling in a crate for seven call sites: the
/// unreserved set of RFC 3986 passes through, everything else — including
/// every byte of a multi-byte character — becomes `%XX`. Space is `%20` and
/// not `+`, which both `serde_urlencoded` and every browser decode the same
/// way in a query.
pub(crate) fn percent_encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Query string carrying the list's current filters, for a link that changes
/// one thing beside them — today the sort headers, which have to keep the
/// filter the operator set while swapping `sort`/`dir`.
///
/// Empty values are dropped, so a link never carries `severity=`, and the
/// result has no leading `?`: the caller appends its own parameter and knows
/// whether a separator is needed.
pub(crate) fn filter_qs(pairs: &[(&str, Option<String>)]) -> String {
    pairs
        .iter()
        .filter_map(|(k, v)| {
            let v = v.as_deref().filter(|v| !v.is_empty())?;
            Some(format!("{k}={}", percent_encode_query(v)))
        })
        .collect::<Vec<_>>()
        .join("&")
}

pub(crate) fn parse_optional_count(raw: &str, field: &str) -> Result<Option<i64>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.parse::<i64>() {
        Ok(n) if (0..=MAX_OPTIONAL_COUNT).contains(&n) => Ok(Some(n)),
        // Distinguished from "not a number": a value that parsed but is
        // absurd deserves to be told so, not to be called a typo.
        Ok(n) if n >= 0 => Err(i18n::lookup(
            i18n::current_lang(),
            "monitoring.shared.count_out_of_range",
        )
        .replace("{field}", field)
        .replace("{max}", &MAX_OPTIONAL_COUNT.to_string())),
        _ => Err(i18n::lookup(
            i18n::current_lang(),
            "monitoring.shared.count_must_be_number",
        )
        .replace("{field}", field)),
    }
}

/// Read-modify-write of one `device_meta` row, for the per-device metadata
/// forms (devices.backend.md §3, WP0.2).
///
/// Read-modify-write rather than a plain upsert, for two reasons the storage
/// layer cannot decide on a route's behalf:
///
/// - `Storage::upsert_device_meta` writes **every** mutable column from
///   `excluded`, so a form carrying only its own scope's fields would null
///   the other scope's override on the way past. Each form edits what it
///   shows and inherits the rest from the row that is already there.
/// - `created_at` and `added_by` are a pair — the registration moment and
///   who registered it. The repo already refuses to rewrite `created_at`;
///   `added_by` has no such guard, so it is preserved here and stamped only
///   when the row is created. A description edit does not make the editor
///   the registrant.
pub(crate) async fn edit_device_meta(
    state: &AppState,
    actor: &CurrentUser,
    hostname: &str,
    scope: &str,
    entity: &str,
    edit: impl FnOnce(&mut portal_storage::NewDeviceMeta),
) -> Result<(), portal_storage::StorageError> {
    let existing = ({
        let a0 = hostname.to_string();
        let a1 = scope.to_string();
        let a2 = entity.to_string();
        state.db.run(move |s| s.get_device_meta(&a0, &a1, &a2))
    })
    .await?;

    let mut row = portal_storage::NewDeviceMeta::new(hostname, scope, entity);
    match existing {
        Some(prev) => {
            row.name = prev.name;
            row.description = prev.description;
            row.added_by = prev.added_by;
            row.offline_k_override = prev.offline_k_override;
            row.backfill_auto = prev.backfill_auto;
            // V14 (B6). Carried like every other column: a form that does
            // not show the serial must not null it, which is the same rule
            // `a_save_does_not_null_the_column_its_form_does_not_show`
            // holds for the two overrides.
            row.serial = prev.serial;
            row.location = prev.location;
            row.store_interval_override = prev.store_interval_override;
        }
        None => row.added_by = Some(actor.username.clone()),
    }

    edit(&mut row);
    state.db.write(move |s| s.upsert_device_meta(&row)).await
}

/// Drops one device's `device_meta` row, because the device itself is gone
/// (devices.backend.md §19, F7).
///
/// `device_meta` is keyed by `(hostname, scope, entity)` and both key spaces
/// are reused — a MAC ends up on another physical Beacon, a `dev_eui` is
/// re-registered. A row left behind is therefore silently inherited by the
/// *next* device at that address: its description, and worse its
/// `offline_k_override`, which puts hardware nobody has configured on a
/// liveness threshold nobody chose for it.
///
/// Best-effort by design. The device registration is already gone from the
/// Hub by the time this runs, and reporting that removal as a failure
/// because a local row survived would be the worse answer — so a failure is
/// logged and swallowed, the same stance every audit write takes.
///
/// **Callers:** `beacon_routes::remove_beacon`. B4's Node removal
/// (`POST /nodes/{node_key}/remove`, §7 WP4.4) is the second one: same call
/// with `THRESHOLD_SCOPE_NODE` and the lowercased `node_key`, beside
/// `delete_pending_for_node`.
pub(crate) async fn forget_device_meta(
    state: &AppState,
    hostname: &str,
    scope: &str,
    entity: &str,
) {
    state.store.forget_meta_label(hostname, scope, entity);
    match ({
        let a0 = hostname.to_string();
        let a1 = scope.to_string();
        let a2 = entity.to_string();
        state.db.write(move |s| s.delete_device_meta(&a0, &a1, &a2))
    })
    .await
    {
        Ok(deleted) => tracing::debug!(hostname, scope, entity, deleted, "forgot device metadata"),
        Err(err) => {
            tracing::warn!(%err, hostname, scope, entity, "device removed but its device_meta row survived")
        }
    }
}

/// Drops one device's `field_thresholds` rows on removal, for the same
/// reason as [`forget_device_meta`]: the MAC is reused, and the next Beacon
/// registered at it must not start with bounds nobody set for it (the
/// removal dialog promises they go, decided 2026-09-23). The change history
/// stays. Best-effort in the same way — logged and swallowed.
///
/// **Callers:** `beacon_routes::remove_beacon`.
pub(crate) async fn forget_field_thresholds(
    state: &AppState,
    hostname: &str,
    scope: &str,
    entity: &str,
) {
    match ({
        let a0 = hostname.to_string();
        let a1 = scope.to_string();
        let a2 = entity.to_string();
        state
            .db
            .write(move |s| s.delete_field_thresholds_for_entity(&a0, &a1, &a2))
    })
    .await
    {
        Ok(deleted) => tracing::debug!(hostname, scope, entity, deleted, "forgot field thresholds"),
        Err(err) => {
            tracing::warn!(%err, hostname, scope, entity, "device removed but its thresholds survived")
        }
    }
    // #120: so are its switched-off sensors — a device registered later under the same key
    // starts with every sensor on.
    let (h, sc, e) = (hostname.to_string(), scope.to_string(), entity.to_string());
    if let Err(err) = state
        .db
        .write(move |s| s.delete_disabled_sensors_for_entity(&h, &sc, &e))
        .await
    {
        tracing::warn!(%err, hostname, scope, entity, "device removed but its switched-off sensors survived");
    }
    state.store.forget_disabled_sensors(hostname, scope, entity);
}

/// Writes one sensor's default bands at its registration (plan/portal/threshold_defaults.md
/// §3): switched off, only for fields without a row. A failure is logged, not the
/// registration's: the persist thread offers the same bands again when the Hub's documents
/// name the sensor.
pub(crate) async fn seed_threshold_defaults(
    state: &AppState,
    hostname: &str,
    scope: &'static str,
    entity: &str,
    fields: &[&str],
) {
    let (h, e) = (hostname.to_string(), entity.to_string());
    let fields: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
    let defaults = crate::threshold_defaults::configured();
    if let Err(err) = state
        .db
        .write(move |s| {
            crate::threshold_defaults::seed(
                s,
                &defaults,
                &h,
                scope,
                &e,
                fields.iter().map(String::as_str),
            )
        })
        .await
    {
        tracing::warn!(%err, hostname, scope, entity, "default thresholds not written at registration");
    }
}

/// Shared scaffolding for route-module integration tests (M6-F): a real
/// `AppState` on a temp dir (superuser `admin`/`correct-horse-battery`
/// pre-seeded), plus login/user helpers. Builds the state directly instead
/// of going through `http::app` so each test module can merge just its own
/// router — the auth extractors still run (they hang off the state), only
/// the app-level middleware (setup gate, origin guard) is out of scope
/// here, matching what these tests assert (role gates + redirects).
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;

    use crate::alarms::AlarmService;
    use crate::auth::{AuthService, NewUserInput, Role};
    use crate::backfill::BackfillService;
    use crate::db::Db;
    use crate::http::AppState;
    use crate::keys::KeyService;
    use crate::relay::CommandRelay;
    use crate::state::StateStore;

    /// A shutdown receiver that never fires, for harnesses with no server to
    /// drain. The sender is leaked on purpose: dropping it makes
    /// `watch::Receiver::changed()` resolve, which `sse::run` correctly reads
    /// as "shut down" and would end a stream under test.
    pub(crate) fn idle_shutdown() -> tokio::sync::watch::Receiver<bool> {
        let (tx, rx) = tokio::sync::watch::channel(false);
        std::mem::forget(tx);
        rx
    }

    /// Returns the state alongside the `TempDir` guard — keep the guard
    /// bound in the caller so the backing directory lives for the test.
    pub(crate) async fn test_state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8 path").to_string();

        let auth = AuthService::open(&data_dir, None, crate::durable::test_gate(&data_dir))
            .expect("open auth service");
        auth.create_superuser(
            "admin".to_string(),
            "correct-horse-battery".to_string(),
            None,
        )
        .await
        .expect("create superuser");

        let db = Db::open(&data_dir, None, "test").expect("open db");
        let cluster = crate::cluster::Registry::load(db.clone());
        let store = Arc::new(StateStore::new());
        let alarms = AlarmService::test_instance(&data_dir);
        // Nudging the alarm table like the real one; no notifier behind it,
        // so the e-mail jobs a test does not assert on go nowhere.
        let calibration = crate::calibration::CalibrationService::test_instance(
            db.clone(),
            &data_dir,
            Arc::clone(&store),
        )
        .wire(
            tokio::sync::mpsc::unbounded_channel().0,
            alarms.nudge_sender(),
        );
        let state = AppState {
            store,
            templates: crate::render::Templates::new("templates", "static"),
            templates_available: true,
            static_dir: "static".to_string(),
            data_dir: data_dir.clone(),
            auth: auth.clone(),
            db: db.clone(),
            sealer: crate::notify::NotifySealer::new(None),
            spill: crate::durable::test_gate(&data_dir),
            relay: CommandRelay::test_instance(),
            // Inert in the test harness: there is no rendered radio config, so
            // the seam answers `NotConfigured` — the routes under test never
            // reach the LoRaWAN arm.
            radio: crate::radio::RadioControl::from_env(),
            keys: KeyService::open(&data_dir, None).expect("open key service"),
            backfill: BackfillService::test_instance(&data_dir),
            beacon_history: crate::beacon_history::BeaconHistoryService::test_instance(&data_dir),
            pending: crate::pending::PendingService::open(&data_dir, None)
                .expect("open pending service"),
            signal_quality: Default::default(),
            beacons: Default::default(),
            offline: crate::offline::OfflineService::test_instance(db),
            calibration,
            alarms,
            maintenance: Some(crate::retention::Maintenance::test_instance(&data_dir)),
            cookie_secure: false,
            local_hostname: "PORTAL-TEST-HOST".to_string(),
            experimental_features_enabled: false,
            shutdown: idle_shutdown(),
            memos: Default::default(),
            cluster,
            // No CA in the harness unless a test writes one here.
            ca_cert_path: Arc::new(dir.path().join("tls").join("ca.crt")),
            reveals: Default::default(),
            sandbox: std::sync::Arc::new(crate::sandbox::SandboxStatus {
                state: crate::sandbox::SandboxState::Unsupported,
                detail: "test harness".into(),
            }),
        };
        (state, dir)
    }

    /// Logs `username` in and returns the session token for a Cookie header.
    pub(crate) async fn login(state: &AppState, username: &str, password: &str) -> String {
        let (token, _user) = state
            .auth
            .login(username.to_string(), password.to_string(), None)
            .await
            .expect("login");
        token
    }

    /// Creates a user with `role` (acting as the seeded superuser) and
    /// returns their session token.
    pub(crate) async fn login_as_role(state: &AppState, role: Role, username: &str) -> String {
        let admin_token = login(state, "admin", "correct-horse-battery").await;
        let admin = state
            .auth
            .validate(&admin_token)
            .await
            .expect("validate admin session");
        state
            .auth
            .create_user(
                &admin,
                NewUserInput {
                    username: username.to_string(),
                    password: "password-1234".to_string(),
                    full_name: None,
                    job_title: None,
                    user_role: role,
                },
            )
            .await
            .expect("create user");
        login(state, username, "password-1234").await
    }
}

#[cfg(test)]
mod tests {
    /// Old `?tab=` links keep working: the tab moves into the path, the
    /// rest of the query stays, an unknown tab lands on Overview.
    #[test]
    fn a_legacy_tab_query_moves_into_the_path() {
        let loc = |raw: Option<&str>| {
            let r = super::device_tab_redirect("/nodes/aa11", raw);
            r.headers()["location"].to_str().unwrap().to_string()
        };
        assert_eq!(loc(None), "/nodes/aa11/overview");
        assert_eq!(
            loc(Some("tab=settings&ok=meta")),
            "/nodes/aa11/settings?ok=meta"
        );
        assert_eq!(
            loc(Some("range=7d&tab=history")),
            "/nodes/aa11/history?range=7d"
        );
        assert_eq!(loc(Some("tab=bogus")), "/nodes/aa11/overview");
    }

    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use portal_storage::AuditFilter;
    use tower::ServiceExt;

    use super::test_support::{login, login_as_role, test_state};
    use super::*;

    /// HS-9: a code the catalogue knows is its message; an unknown one is nothing.
    #[test]
    fn only_a_known_error_code_is_shown() {
        assert!(groups_error("field_taken").is_some());
        assert_eq!(groups_error("bogus"), None);
    }
    use crate::auth::{Role, SESSION_COOKIE};

    /// The bound exists so a count the liveness arithmetic multiplies cannot
    /// overflow.
    #[test]
    fn optional_count_rejects_absurd_values_without_calling_them_typos() {
        assert_eq!(parse_optional_count("", "offline_k"), Ok(None));
        assert_eq!(parse_optional_count("0", "offline_k"), Ok(Some(0)));
        assert_eq!(
            parse_optional_count(&MAX_OPTIONAL_COUNT.to_string(), "offline_k"),
            Ok(Some(MAX_OPTIONAL_COUNT)),
            "the bound itself is accepted"
        );

        // One past the bound, and the value that used to overflow.
        for raw in [
            (MAX_OPTIONAL_COUNT + 1).to_string(),
            "200000000000000000".to_string(),
            i64::MAX.to_string(),
        ] {
            let err = parse_optional_count(&raw, "offline_k")
                .expect_err(&format!("{raw} must be refused"));
            assert!(
                err.contains("offline_k") && err.contains("1000000"),
                "out-of-range message names the field and the bound: {err}"
            );
        }

        // Still distinguished from a genuine typo.
        for raw in ["-1", "abc", "1.5"] {
            assert!(
                parse_optional_count(raw, "offline_k").is_err(),
                "{raw} is refused"
            );
        }
    }

    fn all_audit() -> AuditFilter {
        AuditFilter {
            event_type: None,
            username: None,
            since: None,
            until: None,
            success: None,
            search: None,
            limit: 100,
            offset: 0,
            ..Default::default()
        }
    }

    /// The audit seam WP0.1 routes every privileged device action through:
    /// actor, target and a JSON `details` object land on the row, and
    /// `success` carries the action's real outcome rather than a literal
    /// `true`.
    #[tokio::test]
    async fn audit_action_records_actor_target_and_outcome() {
        let (state, _dir) = test_state().await;
        let token = login(&state, "admin", "correct-horse-battery").await;
        let actor = state.auth.validate(&token).await.expect("validate");

        audit_action(
            &state,
            &actor,
            "node_threshold_set",
            "70b3d5e050011234",
            serde_json::json!({ "field": "temperature", "status": "error" }),
            false,
        )
        .await;

        let (rows, _total) = state
            .auth
            .audit_page(all_audit())
            .await
            .expect("audit page");
        let row = rows
            .iter()
            .find(|r| r.event_type == "node_threshold_set")
            .expect("the audit row was written");

        assert_eq!(row.username.as_deref(), Some("admin"));
        assert_eq!(row.user_id, Some(actor.id));
        assert_eq!(row.target.as_deref(), Some("70b3d5e050011234"));
        assert!(
            !row.success,
            "a failed action must not be audited as success"
        );

        let details: serde_json::Value =
            serde_json::from_str(row.details.as_deref().expect("details present"))
                .expect("details is a JSON object");
        assert_eq!(details["field"], "temperature");
        assert_eq!(details["status"], "error");
    }

    /// The role gate is an extractor, so it runs *before* the handler body —
    /// a refused request must therefore leave no audit row at all, and no
    /// side effect behind it. Asserted because the opposite (auditing the
    /// attempt) would be a defensible design that this one deliberately is
    /// not: `audit_log` records what was done, and the 403 itself is what a
    /// reviewer looks for in the access log.
    #[tokio::test]
    async fn a_refused_request_never_reaches_the_audited_body() {
        let (state, _dir) = test_state().await;
        let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
        let router = node_routes::router().with_state(state.clone());

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/nodes/70b3d5e050011234/refresh-config")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={viewer}"))
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let (rows, _total) = state
            .auth
            .audit_page(all_audit())
            .await
            .expect("audit page");
        assert!(
            !rows.iter().any(|r| r.event_type == "node_config_refresh"),
            "a 403'd request must not write an audit row: {rows:?}"
        );
    }
}
