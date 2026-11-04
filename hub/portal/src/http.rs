//! axum HTTP layer: thin handlers, no business logic (per plan/viewer/stack.md).

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{FromRef, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use tokio::sync::{mpsc, watch};
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::alarms::AlarmService;
use crate::auth::{self, AuthService, CurrentUser};
use crate::backfill::BackfillService;
use crate::config::PortalConfig;
use crate::db::Db;
use crate::i18n;
use crate::keys::KeyService;
use crate::notify::NotifySealer;
use crate::offline::OfflineService;
use crate::relay::CommandRelay;
use crate::render;
use crate::retention::Maintenance;
use crate::routes;
use crate::sandbox::SandboxStatus;
use crate::sse;
use crate::state::StateStore;
use portal_storage::SpillGate;

/// Shared handler state. Phase 1 is single-device (design.md §2): there is
/// no device picker, and no "current device" field here either — the
/// dashboard just asks the store's `PortalState.devices` `BTreeMap` for its
/// first entry on every read (`render::dashboard_ctx`). That's the
/// "derive-on-read" option: simpler than caching the hostname behind an
/// `RwLock` and keeping it in sync as devices come and go, and cheap enough
/// at M1's single-digit-device scale (same cost argument `StateStore::snapshot`
/// already makes).
///
/// M2-C addition: `auth` + `cookie_secure`. `AuthService` is `Clone` (an
/// `Arc<Mutex<Storage>>` inside), so cloning `AppState` per-request is cheap.
///
/// M3-C addition: `db` — the request path's connection (`crate::db::Db`,
/// cheap to clone), used by every route that reads or writes the database.
///
/// M4-C addition: `relay` — the signed-command relay (`crate::relay`), also
/// cheap to clone (`Arc`-backed maps + a broadcast sender, see
/// `CommandRelay`'s doc comment). `sse::events_handler` subscribes its
/// status broadcast alongside `store`'s `ChangeEvent`s.
///
/// M4-D addition: `keys` — the device CA / per-user signing key custody
/// service (`crate::keys::KeyService`), same cheap-clone shape as `auth`/
/// `db` (its own connection). Used by
/// `routes::hub_routes` (pairing, system commands, wired-line thresholds)
/// to mint the signing key every `relay.execute` call needs.
///
/// M5-F addition: `backfill` — the history-backfill service
/// (`crate::backfill::BackfillService`), same cheap-clone shape (its own
/// `Arc<Mutex<Storage>>` connection, #5). The Node backfill routes call
/// `request`/`cancel`/`jobs_for`; `sse::events_handler` subscribes its
/// nudge broadcast to re-render `partials/backfill_jobs.html`.
///
/// M6-E addition: `alarms` — the alarm read/ack/note API
/// (`crate::alarms::AlarmService`), same cheap-clone shape (its own
/// `Arc<Mutex<Storage>>` connection, #7, after backfill). `routes::
/// alarm_routes` calls `list`/`detail`/`acknowledge`/`add_note`;
/// `sse::events_handler` subscribes its nudge broadcast to re-render
/// `partials/alarm_table.html`.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<StateStore>,
    pub templates: render::Templates,
    pub templates_available: bool,
    pub static_dir: String,
    /// `PortalConfig::data_dir` (`/data/portal` on a device), the one tree the portal may
    /// write. No route writes it since `/setup` stopped writing the transport request
    /// (decision #44); tests still read it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub data_dir: String,
    pub auth: AuthService,
    /// The request path's database connection (`crate::db`). Every route
    /// reads through it and the operator-clicked writes (groups, device
    /// metadata, notification settings, manual events) go through it too;
    /// the long-lived writers (`persist`, `pending`, `backfill`, `alarms`)
    /// keep their own connections so a multi-minute task never queues a
    /// page render.
    pub db: Db,
    /// Seals notification secrets (SMTP/webhook credentials); derived from
    /// the same `db_key` the connections are opened with.
    pub sealer: NotifySealer,
    /// The spill journal, for the one request-path write that must not be
    /// lost to a locked database: the notification delivery record
    /// (`crate::durable::insert_report_history`).
    pub spill: Arc<SpillGate>,
    pub relay: CommandRelay,
    /// The in-process `control.radio` seam (plan/portal/p2p_integration.md,
    /// PI2). The v2 Node config/downlink path calls `control_radio::dispatch`
    /// through this instead of relaying a signed command to the v1 `fiber_app`
    /// agent. Cheap to clone (an `Arc` inside), and inert on a Hub with no
    /// rendered radio config — so it costs nothing where it does not apply.
    pub radio: crate::radio::RadioControl,
    pub keys: KeyService,
    pub backfill: BackfillService,
    /// Beacon archive history jobs (plan/portal/beacon_history.md §4).
    pub beacon_history: crate::beacon_history::BeaconHistoryService,
    pub alarms: AlarmService,
    /// B4's Node configuration draft (devices.backend.md §7 WP4.2) — its
    /// own `Arc<Mutex<Storage>>` connection, the same cheap-clone shape
    /// `BackfillService` has and for the same reason: the send walk is a
    /// detached task that lives for minutes and writes repeatedly over that
    /// span, which has no business queueing on a request-path handle. The
    /// lock is never held across an await: each write takes it for one
    /// statement, on the blocking pool except for the interrupt guard's
    /// `Drop`, which takes it on a runtime thread (safe because every other
    /// holder is on the blocking pool — see `PendingService`).
    /// `sse::events_handler` subscribes its nudge broadcast to re-render
    /// `partials/pending_config.html`.
    pub pending: crate::pending::PendingService,
    /// B2's per-device offline verdict (devices.backend.md §5) — the read
    /// side of the sweep in `crate::offline`. Carried on the state, not
    /// looked up per request, because the fault indicator needs it on every
    /// Node/Beacon render including every SSE row swap.
    pub offline: OfflineService,
    /// Calibration certificates and their reminder (calibration.cs.md):
    /// the card's reads and writes, and the level cache the Node and
    /// Beacon rows draw their calibration icon from on every render,
    /// including every SSE row swap — why it is on the state.
    pub calibration: crate::calibration::CalibrationService,
    /// From `PortalConfig::signal_quality` — the floors behind the derived
    /// link-quality indicator. On the state because every Node row needs
    /// them, including every SSE row swap.
    pub signal_quality: crate::config::SignalQualityConfig,
    /// From `PortalConfig::beacons` — the fleet store interval and offline
    /// multiplier. On the state for the same reason `signal_quality` is:
    /// the Beacon detail page shows the YAML default a blank per-Beacon
    /// override inherits (devices.beacon.md §3.5/§3.6, B5 WP5.4).
    pub beacons: crate::config::BeaconsConfig,
    /// Operator-initiated database maintenance (full VACUUM). `None` when the
    /// retention connection could not be opened — the settings page reports it
    /// as unavailable instead of the process refusing to serve.
    pub maintenance: Option<Maintenance>,
    /// M7: the one confinement attempt this process made. Republished on
    /// `/health` to a signed-in session so a degraded sandbox is externally
    /// visible and cannot be mistaken for a confined one
    /// (plan/security/landlock.md §8).
    pub sandbox: Arc<SandboxStatus>,
    /// From `PortalConfig::cookie_secure` — whether session cookies get the
    /// `Secure` flag.
    pub cookie_secure: bool,
    /// The hostname of the machine this portal process runs on, resolved once
    /// at startup by [`resolve_local_hostname`]. `hub_routes` compares it
    /// against a device's MQTT hostname to decide that the device *is* this
    /// machine, and pair it without a typed code.
    ///
    /// Empty string when the hostname could not be read at all — that never
    /// matches a device hostname, so pairing degrades to the code flow.
    pub local_hostname: String,
    /// From `PortalConfig::experimental_features_enabled` — gates non-core
    /// (post-phase-1) route modules in `app()` and non-core nav entries via
    /// `routes::render()`. See that config field's doc comment.
    pub experimental_features_enabled: bool,
    /// Flips to `true` once a shutdown signal arrives, so `/events` streams
    /// can end themselves.
    ///
    /// Graceful shutdown waits for in-flight responses, and an SSE response is
    /// deliberately infinite — without this, one open browser tab keeps the
    /// process alive until systemd's `TimeoutStopSec` fires SIGKILL. A
    /// subscription is not a request in progress: dropping it costs nothing,
    /// because the browser reconnects on its own.
    pub shutdown: watch::Receiver<bool>,
    /// Answers several requests would each rebuild from the database: ended
    /// alarms' peaks and the page-stats row (see [`crate::memo`]).
    pub memos: Arc<crate::memo::RenderMemos>,
    /// The Server's Client Hub registry (`cluster_clients`, C5), cached so
    /// `known_hub()` answers store ∪ registry synchronously.
    pub cluster: crate::cluster::Registry,
    /// Where `GET /connection/ca` reads the Hub CA, and so where the join
    /// bundle's fingerprint is taken from: one path, so the two cannot
    /// disagree. `connection_routes::CA_CERT_PATH` everywhere but a debug
    /// build that sets `PROXIMOS_DEV_CA_CERT`, and tests.
    pub ca_cert_path: Arc<std::path::PathBuf>,
    /// Join bundles waiting for their one audited reveal, in memory only
    /// (devices.hub.md §8.4; [`crate::cluster::RevealVault`]).
    pub reveals: crate::cluster::RevealVault,
}

/// [`AppState::ca_cert_path`]: the constant, or in a debug build the file
/// `PROXIMOS_DEV_CA_CERT` names, so the dev loop can exercise Add hub
/// without a `/data/tls`. A release build never reads the variable
/// (plan/security/landlock.md §2: no paths from configuration).
pub fn ca_cert_path() -> std::path::PathBuf {
    #[cfg(debug_assertions)]
    if let Some(dev) = std::env::var_os("PROXIMOS_DEV_CA_CERT") {
        return std::path::PathBuf::from(dev);
    }
    std::path::PathBuf::from(routes::connection_routes::CA_CERT_PATH)
}

/// Lets `CurrentUser`/`RequireAdmin` (both bound on `AuthService: FromRef<S>`)
/// work as extractors against `AppState`.
impl FromRef<AppState> for AuthService {
    fn from_ref(state: &AppState) -> Self {
        state.auth.clone()
    }
}

/// Lets route handlers extract `State<CommandRelay>` directly instead of
/// reaching through the full `AppState`.
impl FromRef<AppState> for CommandRelay {
    fn from_ref(state: &AppState) -> Self {
        state.relay.clone()
    }
}

/// Lets route handlers extract `State<KeyService>` directly instead of
/// reaching through the full `AppState`.
impl FromRef<AppState> for KeyService {
    fn from_ref(state: &AppState) -> Self {
        state.keys.clone()
    }
}

/// Lets route handlers extract `State<BackfillService>` directly instead of
/// reaching through the full `AppState`.
impl FromRef<AppState> for BackfillService {
    fn from_ref(state: &AppState) -> Self {
        state.backfill.clone()
    }
}

/// Lets route handlers extract `State<AlarmService>` directly instead of
/// reaching through the full `AppState`.
impl FromRef<AppState> for AlarmService {
    fn from_ref(state: &AppState) -> Self {
        state.alarms.clone()
    }
}

/// This machine's hostname: `local_hostname` from the config if set,
/// otherwise `gethostname(2)` — lowercased either way (decision #21).
///
/// Resolved once, at router build time, and logged — the local-vs-remote
/// pairing decision it feeds is otherwise invisible, and "why is it still
/// asking for a code?" is answered by this line.
pub(crate) fn resolve_local_hostname(config: &PortalConfig) -> String {
    match &config.local_hostname {
        Some(configured) => {
            tracing::info!(
                hostname = %configured,
                "portal host identity from config `local_hostname`"
            );
            configured.trim().to_ascii_lowercase()
        }
        None => {
            let resolved = gethostname::gethostname().to_string_lossy().into_owned();
            tracing::info!(hostname = %resolved, "portal host identity from gethostname(2)");
            resolved.trim().to_ascii_lowercase()
        }
    }
}

/// Builds the router. Template rendering degrades gracefully when
/// `templates_dir` doesn't exist yet (mirrors the M0 fallback).
#[allow(clippy::too_many_arguments)]
pub fn app(
    config: &PortalConfig,
    store: Arc<StateStore>,
    auth: AuthService,
    db: Db,
    spill: Arc<SpillGate>,
    relay: CommandRelay,
    // The in-process radio seam, built once in `main` and shared with the backfill service.
    radio: crate::radio::RadioControl,
    keys: KeyService,
    backfill: BackfillService,
    beacon_history: crate::beacon_history::BeaconHistoryService,
    alarms: AlarmService,
    pending: crate::pending::PendingService,
    offline: OfflineService,
    calibration: crate::calibration::CalibrationService,
    maintenance: Option<Maintenance>,
    sandbox: Arc<SandboxStatus>,
    shutdown: watch::Receiver<bool>,
    // `pair/invite` events from the ingest loop (GitLab #61). Consumed here rather than in
    // `main` because the pairing needs the whole `AppState` — the relay, the key store and the
    // audit log — and this is where that exists. `None` in tests, which build the router with no
    // ingest loop behind it.
    pair_invites: Option<mpsc::UnboundedReceiver<crate::state::IngestEvent>>,
) -> Router {
    let templates_available = Path::new(&config.templates_dir).is_dir();

    crate::auth::set_trusted_proxies(&config.trusted_proxies);
    let cluster = crate::cluster::Registry::load(db.clone());
    let state = AppState {
        store,
        offline,
        calibration,
        signal_quality: config.signal_quality,
        beacons: config.beacons.clone(),
        templates: render::Templates::new(&config.templates_dir, &config.static_dir),
        templates_available,
        static_dir: config.static_dir.clone(),
        data_dir: config.data_dir.clone(),
        auth,
        db,
        sealer: NotifySealer::new(config.db_key.as_deref()),
        spill,
        relay,
        radio,
        keys,
        backfill,
        beacon_history,
        alarms,
        pending,
        maintenance,
        sandbox,
        cookie_secure: config.cookie_secure,
        local_hostname: resolve_local_hostname(config),
        experimental_features_enabled: config.experimental_features_enabled,
        shutdown,
        memos: Default::default(),
        cluster,
        ca_cert_path: Arc::new(ca_cert_path()),
        reveals: Default::default(),
    };

    let static_dir = state.static_dir.clone();

    let router = Router::new()
        .route("/", get(dashboard_handler))
        .route("/health", get(health_handler))
        .route("/events", get(sse::events_handler))
        .route("/stats/{scope}", get(stats_fragment))
        .merge(routes::auth_routes::router())
        .merge(routes::user_routes::router())
        .merge(routes::audit_routes::router())
        .merge(routes::history_routes::router())
        .merge(routes::hub_routes::router())
        .merge(routes::cluster_routes::router())
        .merge(routes::docs_routes::router())
        .merge(routes::node_routes::router())
        .merge(routes::beacon_routes::router())
        .merge(routes::calibration_routes::router())
        .merge(routes::group_routes::router())
        .merge(routes::probe_routes::router())
        .merge(routes::alarm_routes::router())
        .merge(routes::notification_routes::router())
        .merge(routes::settings_routes::router())
        .merge(routes::limits_routes::router())
        .merge(routes::lang_routes::router())
        .merge(routes::connection_routes::router());

    // Non-core (post-phase-1, see `PortalConfig::experimental_features_enabled`)
    // route modules merge only under that flag, so they 404 via the `fallback`
    // below when it's off — e.g. the future M10 LoRaWAN cluster / LoRa P2P UI
    // (`plan/portal/control.radio.md`):
    //   if config.experimental_features_enabled {
    //       router = router.merge(routes::radio_routes::router());
    //   }
    // A phase-1-core module that grows a few extra experimental fields/actions
    // gates those inline in its own handlers instead — no wrapper module needed
    // just to hang a flag check off of it.

    // C5: a registered Client turns `complete` when its status reaches this broker
    // (plan/control/cluster.md §9).
    tokio::spawn(crate::cluster::watch_liveness(
        state.store.clone(),
        state.cluster.clone(),
    ));

    if let Some(rx) = pair_invites {
        tokio::spawn(routes::hub_routes::serve_pair_invites(state.clone(), rx));
    }

    router
        // Before `with_state`, so the fallback can use `AppState` extractors
        // like every other handler.
        .fallback(not_found_handler)
        .with_state(state.clone())
        .nest_service(
            "/static",
            Router::new()
                // The brand's logo and favicon come from the binary, not from
                // `static_dir`, so the packaged files cannot disagree with the
                // brand the binary was built for (plan/portal/branding.md §3).
                .route("/logo.svg", get(brand_logo))
                .route("/favicon.ico", get(brand_favicon))
                .fallback_service(ServeDir::new(static_dir))
                .layer(middleware::from_fn_with_state(
                    state.templates.clone(),
                    static_cache_control,
                )),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            forbidden_page,
        ))
        .layer(middleware::from_fn_with_state(state, setup_gate))
        .layer(middleware::from_fn(auth::origin_guard))
        // M9: resolves `lang` once per request and scopes it for
        // `routes::render()`/`dashboard_handler` to pick up — see
        // `i18n::lang_middleware`'s doc comment.
        .layer(middleware::from_fn(i18n::lang_middleware))
        // C36-11: every audit row of the request gets the client address.
        .layer(middleware::from_fn(auth::client_ip_middleware))
        // GitLab #108: outside every layer above that answers by itself (the
        // 403 page, the `/setup` redirect, the cross-origin refusal), so
        // those responses are covered too.
        .layer(middleware::from_fn(no_store_by_default))
        // `/nodes/{key}/` and the like: one canonical path, without the slash.
        .layer(middleware::from_fn(strip_trailing_slash))
        // GitLab #106: outside the portal's own middleware, so the responses they
        // make themselves (setup_gate's redirect, origin_guard's 403, the 403
        // page) carry the headers as well as pages, fragments, /static and 404s.
        .layer(middleware::from_fn(security_headers))
        .layer(TraceLayer::new_for_http())
        .layer(CompressionLayer::new())
}

/// `/static/logo.svg`: the build's brand logo (`portal/branding/<brand>/logo.svg`).
async fn brand_logo(headers: axum::http::HeaderMap) -> Response {
    brand_asset(&headers, "image/svg+xml", proximos_brand::LOGO_SVG)
}

/// `/static/favicon.ico`: the build's brand favicon.
async fn brand_favicon(headers: axum::http::HeaderMap) -> Response {
    brand_asset(&headers, "image/x-icon", proximos_brand::FAVICON_ICO)
}

/// A compiled-in brand file, with an ETag of its contents so the `no-cache` revalidation
/// `static_cache_control` asks for is a 304 rather than the whole file, as `ServeDir` gave
/// the file it replaces.
fn brand_asset(
    headers: &axum::http::HeaderMap,
    content_type: &'static str,
    bytes: &'static [u8],
) -> Response {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let etag = format!("\"{:x}-{:x}\"", hasher.finish(), bytes.len());
    let etag_value = axum::http::HeaderValue::from_str(&etag).expect("hex is a header value");
    let fresh = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"));
    if fresh {
        return (
            StatusCode::NOT_MODIFIED,
            [(axum::http::header::ETAG, etag_value)],
        )
            .into_response();
    }
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static(content_type),
            ),
            (axum::http::header::ETAG, etag_value),
        ],
        bytes,
    )
        .into_response()
}

/// `Cache-Control` for `/static/*` (UX-030). A URL carrying this build's
/// `?v=` stamp (`render::Templates::asset_v`, a hash of the files) never
/// changes content, so the browser may keep it for a year without asking.
/// Anything else (fonts, images, the PDFs, and a `?v=` from another build,
/// D8) is `no-cache`: kept, but revalidated before use, so an upgrade
/// reaches it on the next load. A stale stamp marked immutable would pin
/// this build's file under the old build's URL for a year, the day a
/// downgrade or a page cached across an upgrade asks for it. With no header
/// at all the browser guesses a freshness from the file's age, and ran an
/// 11-day-old script after an upgrade.
async fn static_cache_control(
    State(templates): State<render::Templates>,
    request: Request,
    next: Next,
) -> Response {
    let versioned = request.uri().query().is_some_and(|q| {
        let current = format!("v={}", templates.asset_v());
        q.split('&').any(|pair| pair == current)
    });
    let mut response = next.run(request).await;
    if response.status().is_success() || response.status() == StatusCode::NOT_MODIFIED {
        let value = if versioned {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static(value),
        );
    }
    response
}

/// `Cache-Control: no-store` and `Pragma: no-cache` on every response that
/// did not set its own `Cache-Control` (GitLab #108). Pages, htmx fragments,
/// CSV and JSON downloads and chart data carry operator details and
/// temperature readings; with no header a browser or a proxy is free to keep
/// them on disk past the session that fetched them. `Pragma` is for HTTP/1.0
/// caches that do not read `Cache-Control`.
///
/// A response that already says how to cache keeps its value: a `/static/*`
/// hit ([`static_cache_control`]), `/events` (axum's SSE `no-cache`),
/// `/connection/ca` (`no-cache`) and the cluster join-bundle reveal
/// (`no-store`). Anonymous responses (`/login`, `/setup`, `/health`, the 404
/// page, redirects) are deliberately not exempt: they are dynamic and small,
/// caching them gains nothing, and one rule without carve-outs cannot miss a
/// page that later starts showing data. The known cost: browsers keep
/// `no-store` pages out of the back/forward cache, so Back reloads the page.
/// A GET or HEAD whose path ends in `/` (other than `/` itself) is sent,
/// 308, to the same path without it, query kept. No route is declared with
/// a trailing slash, so without this `/hubs/{hub}/` and `/nodes/{key}/` are
/// 404s although `/hubs/{hub}` and `/nodes/{key}` are not (2026-10-14).
async fn strip_trailing_slash(request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let is_read = matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    );
    if is_read && path.len() > 1 && path.ends_with('/') {
        let trimmed = path.trim_end_matches('/');
        let trimmed = if trimmed.is_empty() { "/" } else { trimmed };
        let to = match request.uri().query() {
            Some(q) => format!("{trimmed}?{q}"),
            None => trimmed.to_string(),
        };
        return axum::response::Redirect::permanent(&to).into_response();
    }
    next.run(request).await
}

async fn no_store_by_default(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    if !headers.contains_key(axum::http::header::CACHE_CONTROL) {
        headers.insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
        headers.insert(
            axum::http::header::PRAGMA,
            axum::http::HeaderValue::from_static("no-cache"),
        );
    }
    response
}

/// `Content-Security-Policy` on every response (GitLab #106, design.md §4
/// amendment 2026-10-10). Everything the portal loads is its own: scripts,
/// styles, fonts and images come from `/static`, `/events` and the chart
/// fetches are same-origin, and nothing is framed or embedded. So every source
/// is `'self'`, plus `data:` images for the SVG backgrounds and masks inlined
/// in `app.css`. The page may not be framed (as nginx's `X-Frame-Options:
/// DENY` already says), change its base URL, or post a form to another origin.
///
/// It is not a strict CSP yet. Two script allowances stay because the
/// templates need them today:
///
/// - `'unsafe-inline'`: inline `<script>` blocks (`base.html`, most pages,
///   partials swapped in over SSE) and `on*=` handler attributes.
/// - `'unsafe-eval'`: htmx compiles `hx-on` attributes with `Function()`, and
///   the standard Alpine build evaluates every `x-*` expression the same way.
///
/// `style-src 'unsafe-inline'` covers `style=` attributes, Alpine's `:style`,
/// the indicator `<style>` htmx injects and the ECharts tooltip markup.
/// Dropping the script allowances needs per-request nonces (`routes::render`,
/// htmx `inlineScriptNonce`), the inline handlers moved into static scripts,
/// Alpine's CSP build and `htmx.config.allowEval = false`; that is a follow-up.
/// What this policy already stops: scripts, styles, images and connections
/// from other origins, plugins, `<base>` hijacking, off-site form posts and
/// framing.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
    script-src 'self' 'unsafe-inline' 'unsafe-eval'; \
    style-src 'self' 'unsafe-inline'; \
    img-src 'self' data:; \
    font-src 'self'; \
    connect-src 'self'; \
    object-src 'none'; \
    base-uri 'self'; \
    form-action 'self'; \
    frame-ancestors 'none'";

/// `Permissions-Policy` on every response (GitLab #106). The portal uses none
/// of these features, so it turns them off for itself and for anything it
/// might ever embed. Only names browsers recognise are listed, because an
/// unknown one is a console warning on every page: `bluetooth` is left out for
/// that reason (Brave on Chromium 154, Linux, reports "Unrecognized feature").
/// The clipboard copy on the Beacon page needs no entry, because
/// `clipboard-write` already defaults to `self`.
const PERMISSIONS_POLICY: &str = "camera=(), microphone=(), geolocation=(), usb=(), payment=()";

/// Adds [`CONTENT_SECURITY_POLICY`] and [`PERMISSIONS_POLICY`] to every
/// response. The portal sets them rather than nginx (`fiber-portal-nginx.conf`)
/// because the policy follows the templates: a template that needs a new
/// source changes the policy in the same commit. The dev loop and the demo
/// behind Caddy also get the same policy as a Hub that way.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        axum::http::HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        axum::http::HeaderName::from_static("permissions-policy"),
        axum::http::HeaderValue::from_static(PERMISSIONS_POLICY),
    );
    response
}

/// A role 403 (`auth::ForbiddenPage` on the response) becomes the error page
/// inside the layout, status kept, so a viewer who opens an admin link has
/// the navigation and a way back instead of a bare heading. htmx requests
/// keep the bare body: a page swapped into a fragment slot would be worse.
async fn forbidden_page(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let htmx = request.headers().contains_key("HX-Request");
    let response = next.run(request).await;
    match response.extensions().get::<auth::ForbiddenPage>() {
        Some(page) if !htmx => {
            let body = routes::render(
                &state,
                "pages/not_found.html",
                render::page_ctx(
                    minijinja::context! {
                        code => 403,
                        title => crate::i18n::lookup(crate::i18n::current_lang(), "common.forbidden.title"),
                        message => page.message.clone(),
                    },
                    Some(&page.user),
                ),
            );
            (StatusCode::FORBIDDEN, body).into_response()
        }
        _ => response,
    }
}

/// Forces every route except `/setup`, `/health`, `/connection/ca` and
/// `/static/*` to redirect to `/setup` while [`AuthService::setup_required`]
/// holds (no superuser yet — a fresh instance can't usefully do anything
/// else). `/connection/ca` is public key material fetched by a program (a
/// joining Client Hub, plan/control/cluster.md §6), not a page for a person.
/// `/setup` itself flips to "already done -> /login" per-handler once setup
/// completes (`routes::auth_routes::setup_page`), not here.
async fn setup_gate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let exempt = path == "/setup"
        || path == "/health"
        || path == routes::connection_routes::CA_ROUTE
        || path.starts_with("/static/");
    if exempt || !state.auth.setup_required().await {
        return next.run(request).await;
    }
    Redirect::to("/setup").into_response()
}

/// `GET /health` — unauthenticated liveness, plus the build version and the
/// sandbox verdict for a signed-in session.
///
/// An anonymous client gets `{"status":"healthy"}` and nothing else (GitLab
/// #104, design.md §4 amendment 2026-10-02): the version and the Landlock error
/// text are of no use to a liveness probe and of some use to an attacker.
///
/// `sandbox.state != "enforced"` is the externally visible half of
/// landlock.md §8's "run unconfined and say so, loudly": the log line happens
/// once at startup, this endpoint keeps saying it to a signed-in operator who
/// checks later. `status` stays `healthy` regardless — confinement is
/// defense-in-depth, not a monitoring function, so a degraded sandbox must not
/// make the device look dead.
async fn health_handler(
    current_user: Option<CurrentUser>,
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    if current_user.is_none() {
        return Json(serde_json::json!({ "status": "healthy" }));
    }
    Json(serde_json::json!({
        "status": "healthy",
        "version": crate::version(),
        "sandbox": &*state.sandbox,
    }))
}

/// Router fallback: an unmatched path gets the 404 page rather than axum's
/// empty body. Reached both by a mistyped URL and — until this existed — by
/// `/settings`, a link the navbar renders itself.
///
/// The user is optional so a logged-in visitor keeps the navbar and can click
/// their way out; anonymous requests are *not* redirected to `/login`, because
/// "this path does not exist" is the honest answer and a redirect would hide it.
async fn not_found_handler(
    current_user: Option<CurrentUser>,
    State(state): State<AppState>,
) -> Response {
    routes::not_found(&state, current_user.as_ref())
}

/// `GET /` — the live dashboard (replaces M0's `pages/index.html`, which is
/// left in place unrouted rather than deleted). Auth-gated (M2-C): an
/// anonymous request never reaches the body below — the `CurrentUser`
/// extractor itself 303s to `/login` first.
/// `?error=<slug>` comes back from a group mutation's redirect (see
/// `routes::group_routes`), and the template turns it into a translated
/// banner.
#[derive(serde::Deserialize)]
struct DashboardQuery {
    error: Option<String>,
}

async fn dashboard_handler(
    current_user: CurrentUser,
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<DashboardQuery>,
) -> Response {
    let error = q
        .error
        .as_deref()
        .filter(|c| !c.is_empty())
        .and_then(routes::groups_error);
    if state.templates_available {
        let portal_state = state.store.snapshot();
        let can_edit = render::can_edit_thresholds(&current_user);
        // Resolved through the same helper the two fragment routes use, so a
        // swapped-in row cannot arrive in a different layout from the page
        // that asked for it.
        let view_mode = crate::monitoring::effective_view_mode(&current_user);
        // Groups, their membership and the Beacon thresholds the rows draw
        // gauges from all live in the database, so the page opens with three
        // reads before it can render a row.
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
        let beacon_thresholds =
            routes::beacon_routes::beacon_threshold_map(&state, &portal_state).await;
        // The detector's verdict table, not the Hub's echo: it feeds both the
        // fault axis and `connected` on every group row, which is what closed
        // §19.1's F2/F3 residue for the monitoring page (MB2, §15).
        //
        // Taken **once** and handed to all three readers — the rows, the
        // member picker and the four header tiles (review round 2 brought the
        // last two onto it). One table also means the three cannot disagree
        // about a device within a single render.
        let liveness = state.offline.liveness_snapshot();
        let mut sections = crate::monitoring::sections(
            &portal_state,
            &groups,
            &members,
            &beacon_thresholds,
            &liveness,
        );
        crate::monitoring::Descriptions::load(&state.db, &portal_state)
            .await
            .add_to_sections(&mut sections);
        // The detailed card and table print the 24 h min/max, so only they
        // pay for the history reads behind it.
        if view_mode == "card-detailed" || view_mode == "table-detailed" {
            crate::monitoring::fill_sparklines(&mut sections, &state.db, render::now_unix()).await;
        }
        // Counted over the rows the groups show (monitoring.md §2.1,
        // amended 2026-09-25), before `sections` moves into the page context.
        let stats = render::monitoring_stats_ctx(portal_state.mqtt.connected, &sections);
        // GitLab #109: the Hub this portal runs on, while it is not paired. Nothing on it can
        // be signed, and after `/setup` nothing on this page said so.
        let unpaired = match routes::cluster_routes::local_server(&state) {
            Some(local) => routes::unpaired_banner(&state, &current_user, &[local], "/").await,
            None => routes::UnpairedBanner::default(),
        };
        let ctx = render::page_ctx(
            render::dashboard_ctx(
                &portal_state,
                render::MonitoringInputs {
                    can_edit,
                    sections,
                    view_mode,
                    error,
                },
            ),
            Some(&current_user),
        );
        // M9: the dashboard renders directly rather than through
        // `routes::render()` (it's the one handler that does), so this is
        // its own single merge point for `lang` — see `i18n.rs`'s module
        // doc for why this reads from the per-request task-local rather
        // than taking a parameter.
        let ctx = minijinja::context! {
            lang => i18n::current_lang(),
            // partials/unpaired_banner.html; an admin pairs from it (#109).
            unpaired => unpaired,
            stats => stats,
            // And the sidebar's alarm pill, for the same reason `lang` is
            // here: bypassing `routes::render()` means bypassing everything
            // it merges in, and a missing count renders the pill as nothing
            // until htmx fills it a moment after the page has painted.
            nav_alarm_count => state.alarms.cached_active_count(),
            // The group CSV dialog opens on the last 24 h, in the display
            // zone its inputs are read in (UX-042).
            export_from => crate::display_tz::datetime_local(render::now_unix() - 24 * 3600),
            export_to => crate::display_tz::datetime_local(render::now_unix()),
            ..ctx
        };
        let rendered = state
            .templates
            .env()
            .get_template("pages/dashboard.html")
            .and_then(|tmpl| tmpl.render(ctx));

        match rendered {
            Ok(html) => return Html(html).into_response(),
            Err(err) => {
                tracing::warn!(
                    %err,
                    "failed to render pages/dashboard.html; falling back to placeholder"
                );
            }
        }
    }

    "portal — templates not installed".into_response()
}

/// The Monitoring strip, counted over the groups the way
/// [`dashboard_handler`] counts it: for `GET /stats/monitoring` and for a
/// page under the Monitoring nav entry that has no sections of its own (the
/// probe detail page).
pub(crate) async fn monitoring_stats(
    state: &AppState,
    portal_state: &crate::state::PortalState,
    liveness: &crate::notify::offline::LivenessVerdicts,
) -> render::PageStatsCtx {
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
    let beacon_thresholds = routes::beacon_routes::beacon_threshold_map(state, portal_state).await;
    let sections = crate::monitoring::sections(
        portal_state,
        &groups,
        &members,
        &beacon_thresholds,
        liveness,
    );
    render::monitoring_stats_ctx(portal_state.mqtt.connected, &sections)
}

/// `GET /stats/{scope}` — the metric row above a page, refetched by the row
/// itself on the SSE `page-stats` nudge (UX-020; monitoring.md §2.1, §7
/// "PageStatsCtx broadcast, debounced"). `scope` is what the row's
/// `PageStatsCtx::refresh` named: `monitoring` counts the groups the way
/// [`dashboard_handler`] does, the rest are `render::StatsScope` slugs. An
/// unknown one answers empty, like the other fragment routes' misses.
async fn stats_fragment(
    current_user: CurrentUser,
    State(state): State<AppState>,
    axum::extract::Path(scope): axum::extract::Path<String>,
) -> Response {
    // Every open tab refetches this on the same SSE nudge, and the
    // `monitoring` row reloads groups, members and Beacon thresholds from the
    // database. The row holds i18n keys, not text, so one build is shared by
    // every tab the nudge reached (`crate::memo::RenderMemos::page_stats`).
    let stats = match state.memos.page_stats.get(&scope) {
        Some(stats) => stats,
        None => {
            let portal_state = state.store.snapshot();
            let liveness = state.offline.liveness_snapshot();
            let stats = if scope == "monitoring" {
                monitoring_stats(&state, &portal_state, &liveness).await
            } else if let Some(scope) = render::StatsScope::from_slug(&scope) {
                render::page_stats_ctx(&portal_state, &liveness, state.alarms.hub_faults(), scope)
            } else {
                return ().into_response();
            };
            state.memos.page_stats.insert(scope, stats.clone());
            stats
        }
    };
    routes::render_fragment(
        &state,
        "partials/page_stats.html",
        render::page_ctx(minijinja::context! { stats => stats }, Some(&current_user)),
    )
}

/// Resolves once ctrl-c or SIGTERM arrives, for `axum::serve`'s graceful shutdown.
/// Serves until a shutdown signal, then drains — with the drain bounded.
///
/// Three things have to happen in this order, and getting it wrong is how both
/// of this function's history's bugs happened:
///
/// 1. `signal` resolves (SIGINT/SIGTERM in production).
/// 2. `shutdown_tx` closes the SSE subscriptions. Graceful shutdown waits for
///    in-flight responses and `/events` never finishes on its own, so without
///    this one open browser tab holds the process until systemd SIGKILLs it.
/// 3. Only *then* does axum start draining, and only then does `grace` start
///    ticking. `grace` bounds the drain, never the serving: arming it up front
///    (e.g. `timeout(grace, server)`) gives the process a lifetime instead of
///    a shutdown budget, which is a bug a short manual test cannot see.
///
/// The `grace` arm cannot fire before shutdown begins, because `armed_rx` stays
/// pending until step 1. Reaching it means a response outlived the budget; a
/// monitoring device restarting late is worse than one unfinished download.
pub async fn serve<S>(
    listener: tokio::net::TcpListener,
    app: Router,
    shutdown_tx: watch::Sender<bool>,
    grace: Duration,
    signal: S,
) -> std::io::Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    let (armed_tx, armed_rx) = tokio::sync::oneshot::channel();

    let begin_shutdown = async move {
        signal.await;
        tracing::info!("shutdown signal received; closing event streams");
        let _ = shutdown_tx.send(true);
        let _ = armed_tx.send(());
    };

    // `with_connect_info` makes `ConnectInfo<SocketAddr>` available to
    // extractors (routes::auth_routes's login/setup IP-for-audit lookup).
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(begin_shutdown);

    tokio::select! {
        result = server => result,
        _ = async move {
            // Pending until shutdown starts; the sleep begins only after.
            let _ = armed_rx.await;
            tokio::time::sleep(grace).await;
        } => {
            tracing::warn!(
                seconds = grace.as_secs(),
                "graceful shutdown did not finish in time; exiting anyway"
            );
            Ok(())
        }
    }
}

pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(%err, "failed to install Ctrl+C handler");
        }
    };

    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::error!(%err, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {

    //! Asserts the unauthenticated surface is exactly `/setup` (while setup
    //! is pending), `/login`, `/logout`, `/health`, `/connection/ca`, and `/static/*` — every
    //! other route must redirect an anonymous request to `/login` (item 8
    //! of the M2-C work package). A superuser is created up front so the
    //! `setup_gate` middleware (item 2) isn't itself what's under test here.

    #[tokio::test]
    async fn a_trailing_slash_redirects_to_the_path_without_it() {
        use axum::body::Body;
        use tower::ServiceExt;
        let app = axum::Router::new()
            .route(
                "/x",
                axum::routing::get(|| async { "x" }).post(|| async { "p" }),
            )
            .layer(axum::middleware::from_fn(super::strip_trailing_slash));
        let send = |method: &str, uri: &str| {
            let req = axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            app.clone().oneshot(req)
        };
        let r = send("GET", "/x/?a=1").await.unwrap();
        assert_eq!(r.status(), axum::http::StatusCode::PERMANENT_REDIRECT);
        assert_eq!(r.headers()["location"], "/x?a=1");
        let r = send("GET", "/x").await.unwrap();
        assert_eq!(r.status(), axum::http::StatusCode::OK);
        // A form post is not redirected: a 308 would replay it, so it 404s.
        let r = send("POST", "/x/").await.unwrap();
        assert_ne!(r.status(), axum::http::StatusCode::PERMANENT_REDIRECT);
    }

    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    use crate::auth::{NewUserInput, Role, SESSION_COOKIE};

    use super::*;

    /// Returns the router alongside the `TempDir` guard — keep the guard
    /// bound in the caller so the backing directory lives for the test.
    /// The regression this pins: `grace` must bound the *drain*, not serving.
    /// An earlier version wrapped the whole server in `timeout(grace, ...)`,
    /// which quietly gave the process a 10-second lifetime — invisible to
    /// every test and to any manual run shorter than the grace period.
    #[tokio::test]
    async fn serving_outlives_the_shutdown_grace() {
        let (app, _dir) = test_app().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral");
        let addr = listener.local_addr().expect("local addr");
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let (signal_tx, signal_rx) = tokio::sync::oneshot::channel::<()>();
        let grace = Duration::from_millis(100);

        let server = tokio::spawn(serve(listener, app, shutdown_tx, grace, async move {
            let _ = signal_rx.await;
        }));

        // Well past the grace period, with no signal sent: still serving.
        tokio::time::sleep(grace * 5).await;
        let health = reqwest::get(format!("http://{addr}/health"))
            .await
            .expect("health request");
        assert_eq!(health.status(), 200, "server died before it was asked to");

        // And it stops when actually asked.
        signal_tx.send(()).expect("send signal");
        let stopped = tokio::time::timeout(Duration::from_secs(5), server).await;
        assert!(
            matches!(stopped, Ok(Ok(Ok(())))),
            "server did not stop on signal: {stopped:?}"
        );
    }

    /// An unknown path renders the 404 page instead of axum's empty body.
    #[tokio::test]
    async fn unknown_path_renders_the_not_found_page() {
        let (app, _dir) = test_app().await;
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/no-such-page")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let body = String::from_utf8(bytes.to_vec()).expect("utf8");
        assert!(body.contains("404"), "not the rendered 404 page: {body}");
    }

    /// Every link the navbar renders must resolve. `/settings` shipped in the
    /// nav with no route behind it, so the app itself pointed at a 404 — the
    /// kind of defect no per-page test catches, because each page is fine.
    #[tokio::test]
    async fn every_nav_link_resolves() {
        let nav = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("templates/layout/nav_items.html"),
        )
        .expect("read nav template");

        // The `("id", "/path", "icon", gate)` tuples of nav_items, role-gated
        // entries included — the gate only decides who SEES a link, not
        // whether the route behind it exists.
        let mut hrefs: Vec<String> = Vec::new();
        for part in nav.split("(\"").skip(1) {
            if let Some((_id, rest)) = part.split_once("\", \"/") {
                if let Some((path, _)) = rest.split_once('"') {
                    hrefs.push(format!("/{path}"));
                }
            }
        }
        hrefs.sort();
        hrefs.dedup();
        assert!(hrefs.len() >= 8, "nav parser found too little: {hrefs:?}");
        // One link named outright, so a parser that quietly stops matching
        // nav.html fails here instead of passing on an empty sweep. It used
        // to be `/settings`, whose whole job was being a link that resolved;
        // that page is retired, so the canary moved to one that carries
        // content.
        assert!(
            hrefs.iter().any(|h| h == "/audit"),
            "nav parser missed /audit: {hrefs:?}"
        );
        // `/settings` is a REDIRECT, not a page — the settings area has no
        // index, only sections. The sweep below asserts "not 404", which a
        // 303 satisfies, so it needs no special case; this is here to say
        // that is deliberate rather than lucky.
        assert!(
            hrefs.iter().any(|h| h == "/settings"),
            "the settings entry left the nav: {hrefs:?}"
        );

        let (app, _dir) = test_app().await;

        // No session on purpose: an existing page 303s to /login for an
        // anonymous visitor, a missing one 404s. That difference is the whole
        // assertion, and it needs no login.
        for href in hrefs {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(&href)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .expect("request");
            assert_ne!(
                response.status(),
                StatusCode::NOT_FOUND,
                "navbar links {href}, which 404s"
            );
        }
    }
    /// Decision #50 (plan/portal/branding.md §9): every page, anonymous and signed in, in both
    /// languages, carries the build's brand in its title and theme — and in any build but the
    /// default one, no "ProXimos" or "PROXIMOS" anywhere in the HTML a browser receives.
    /// `PROXIMOS_BRAND=fiber cargo test` is the run that makes the second half bite.
    #[tokio::test]
    async fn every_page_carries_the_builds_brand() {
        let (app, _dir) = test_app().await;
        let send = |request: Request<Body>| {
            let app = app.clone();
            async move { app.oneshot(request).await.expect("response") }
        };
        let login = send(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    1,
                ))))
                .body(Body::from("username=admin&password=correct-horse-battery"))
                .unwrap(),
        )
        .await;
        let cookie = login
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.starts_with(SESSION_COOKIE))
            .and_then(|v| v.split(';').next())
            .expect("session cookie")
            .to_string();

        let pages = [
            "/login",
            "/",
            "/alarms",
            "/audit",
            "/beacons",
            "/beacons/add",
            "/docs",
            "/hubs",
            "/hubs/add",
            "/limits",
            "/monitoring/groups/new",
            "/nodes",
            "/nodes/add",
            "/settings",
            "/settings/maintenance",
            "/settings/mqtt",
            "/settings/password",
            "/settings/users",
            "/settings/users/new",
            "/trends",
            "/no-such-page",
        ];
        let mut checked = 0;
        for lang in ["en", "cs"] {
            for uri in pages {
                let mut cookies = format!("lang={lang}");
                if uri != "/login" {
                    cookies.push_str(&format!("; {cookie}"));
                }
                let response = send(
                    Request::builder()
                        .uri(uri)
                        .header(header::COOKIE, cookies)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await;
                let status = response.status();
                // A redirect (an empty cluster's /hubs/add, /history's canonical URL) has no page.
                if status.is_redirection() {
                    continue;
                }
                assert!(
                    status == StatusCode::OK || uri == "/no-such-page",
                    "{uri} {lang}: {status}"
                );
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .expect("body");
                let html = String::from_utf8_lossy(&bytes);
                if !html.contains("<title>") {
                    continue;
                }
                assert!(
                    html.contains(&format!("data-theme=\"{}\"", proximos_brand::THEME)),
                    "{uri} {lang}: no {} theme",
                    proximos_brand::THEME
                );
                let title = html
                    .split("<title>")
                    .nth(1)
                    .and_then(|t| t.split("</title>").next())
                    .unwrap_or_default();
                assert!(
                    title.contains(proximos_brand::PRODUCT),
                    "{uri} {lang}: title {title:?} does not name {}",
                    proximos_brand::PRODUCT
                );
                if proximos_brand::ID != "proximos" {
                    for word in ["ProXimos", "PROXIMOS"] {
                        if let Some(at) = html.find(word) {
                            let from = html[..at]
                                .char_indices()
                                .rev()
                                .nth(80)
                                .map_or(0, |(i, _)| i);
                            let to = (at + 80).min(html.len());
                            panic!(
                                "{uri} {lang}: {word} in a {} build: …{}…",
                                proximos_brand::PRODUCT,
                                html.get(from..to).unwrap_or(word)
                            );
                        }
                    }
                }
                checked += 1;
            }
        }
        assert!(checked >= 36, "only {checked} pages rendered");
    }

    /// UX-030: every script and stylesheet a page loads carries the content
    /// stamp, and `/static/*` says how long the browser may keep it — for
    /// good when versioned, revalidate-first when not.
    #[tokio::test]
    async fn pages_version_their_assets_and_static_says_how_to_cache() {
        let (app, _dir) = test_app().await;
        let send = |request: Request<Body>| {
            let app = app.clone();
            async move { app.oneshot(request).await.expect("response") }
        };
        let login = send(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    1,
                ))))
                .body(Body::from("username=admin&password=correct-horse-battery"))
                .unwrap(),
        )
        .await;
        let cookie = login
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.starts_with(SESSION_COOKIE))
            .and_then(|v| v.split(';').next())
            .expect("session cookie")
            .to_string();

        let stamp = crate::render::Templates::new("templates", "static")
            .env()
            .globals()
            .find(|(name, _)| *name == "asset_v")
            .map(|(_, value)| value.to_string())
            .expect("asset_v global");
        assert!(stamp.len() >= 8, "not a content hash: {stamp}");

        // The dashboard (base layout + list script), a chart page, and the
        // anonymous bare layout.
        let mut checked = 0;
        for uri in ["/", "/trends", "/login"] {
            let mut builder = Request::builder().uri(uri);
            if uri != "/login" {
                builder = builder.header(header::COOKIE, &cookie);
            }
            let response = send(builder.body(Body::empty()).unwrap()).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            let html = String::from_utf8_lossy(&bytes);
            for url in html
                .split('"')
                .filter(|s| s.starts_with("/static/js/") || s.starts_with("/static/css/"))
            {
                assert!(
                    url.ends_with(&format!("?v={stamp}")),
                    "{uri} loads {url} without the stamp"
                );
                checked += 1;
            }
        }
        assert!(checked >= 6, "too few asset urls found: {checked}");

        for (uri, expected) in [
            (
                format!("/static/js/portal-charts.js?v={stamp}"),
                "public, max-age=31536000, immutable",
            ),
            ("/static/js/portal-charts.js".to_string(), "no-cache"),
            // D8: a stamp from another build is not this file's content, so
            // it must not be kept for a year under that URL.
            (
                "/static/js/portal-charts.js?v=0123456789abcdef".to_string(),
                "no-cache",
            ),
        ] {
            let response = send(Request::builder().uri(&uri).body(Body::empty()).unwrap()).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(
                response.headers().get(header::CACHE_CONTROL).unwrap(),
                expected,
                "{uri}"
            );
            // GitLab #108: the dynamic responses' default leaves these alone.
            assert!(response.headers().get(header::PRAGMA).is_none(), "{uri}");
        }

        // The brand's logo and favicon come from the binary (plan/portal/branding.md §3),
        // under the same caching rule, and revalidate to a 304.
        for (uri, content_type, bytes) in [
            (
                "/static/logo.svg",
                "image/svg+xml",
                proximos_brand::LOGO_SVG,
            ),
            (
                "/static/favicon.ico",
                "image/x-icon",
                proximos_brand::FAVICON_ICO,
            ),
        ] {
            let response = send(Request::builder().uri(uri).body(Body::empty()).unwrap()).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE).unwrap(),
                content_type
            );
            assert_eq!(
                response.headers().get(header::CACHE_CONTROL).unwrap(),
                "no-cache"
            );
            let etag = response.headers().get(header::ETAG).expect("etag").clone();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            assert_eq!(&body[..], bytes, "{uri}");
            let again = send(
                Request::builder()
                    .uri(uri)
                    .header(header::IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(again.status(), StatusCode::NOT_MODIFIED, "{uri}");
        }
    }

    /// Asserts `response` carries the dynamic-response default (GitLab #108).
    fn assert_no_store(response: &axum::http::Response<Body>, what: &str) {
        let value = |name| response.headers().get(name).and_then(|v| v.to_str().ok());
        assert_eq!(value(header::CACHE_CONTROL), Some("no-store"), "{what}");
        assert_eq!(value(header::PRAGMA), Some("no-cache"), "{what}");
    }

    /// GitLab #108: what a signed-in operator sees — pages with other
    /// operators' details, htmx fragments, CSV downloads — is never kept by
    /// a browser or a proxy. Includes the 403 page `forbidden_page` renders
    /// after the handler ran, which only an outer layer can reach.
    #[tokio::test]
    async fn signed_in_responses_are_not_stored() {
        use crate::state::IngestEvent;
        use fiber_protocol::payloads::status::DeviceStatus;

        // A group export needs a Hub to name its rows after.
        let store = Arc::new(StateStore::new());
        store.apply(IngestEvent::Status {
            hostname: "h1".into(),
            status: DeviceStatus::Online,
        });
        let (app, dir) = test_app_with(store).await;
        let data_dir = dir.path().to_str().expect("utf8 path");
        let admin = session_for(data_dir, Role::Admin, "qa-admin").await;
        let viewer = session_for(data_dir, Role::Viewer, "qa-viewer").await;

        assert_eq!(
            post_as(
                &app,
                "/monitoring/groups",
                Some(&admin),
                "name=Cold&member%5B%5D=probe%3A0",
            )
            .await,
            StatusCode::SEE_OTHER
        );

        let send = |path: &str, token: &str, htmx: bool| {
            let mut builder = Request::builder()
                .uri(path)
                .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"));
            if htmx {
                builder = builder.header("HX-Request", "true");
            }
            app.clone().oneshot(builder.body(Body::empty()).unwrap())
        };
        let (admin, viewer) = (admin.as_str(), viewer.as_str());
        for (path, token, htmx, status) in [
            ("/", admin, false, StatusCode::OK),
            ("/settings/users/1", admin, false, StatusCode::OK),
            ("/stats/hubs", admin, true, StatusCode::OK),
            (
                "/export/group/1.csv?from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z",
                admin,
                false,
                StatusCode::OK,
            ),
            ("/settings/users/1", viewer, false, StatusCode::FORBIDDEN),
        ] {
            let response = send(path, token, htmx).await.expect("response");
            assert_eq!(response.status(), status, "{path}");
            assert_no_store(&response, path);
        }

        // A handler's own policy wins: the SSE stream stays `no-cache`.
        let events = send("/events", admin, false).await.expect("response");
        assert_eq!(events.status(), StatusCode::OK);
        assert_eq!(
            events.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        assert!(events.headers().get(header::PRAGMA).is_none());
    }

    /// GitLab #108: the anonymous surface gets the same default — no
    /// carve-outs to keep in step with the pages — including the 404
    /// fallback and the cross-origin refusal `origin_guard` answers itself.
    #[tokio::test]
    async fn anonymous_responses_are_not_stored() {
        let (app, _dir) = test_app().await;
        for (path, status) in [
            ("/login", StatusCode::OK),
            ("/", StatusCode::SEE_OTHER),
            ("/no/such/page", StatusCode::NOT_FOUND),
        ] {
            let response = get(&app, path).await;
            assert_eq!(response.status(), status, "{path}");
            assert_no_store(&response, path);
        }

        let refused = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/logout")
                    .header(header::HOST, "portal.example")
                    .header(header::ORIGIN, "https://elsewhere.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        assert_no_store(&refused, "cross-origin POST");
    }

    /// GitLab #106: every response carries the CSP and the Permissions-Policy,
    /// including the ones a middleware answers itself (the anonymous redirect,
    /// the role 403 page, the cross-origin refusal), not just rendered pages.
    #[tokio::test]
    async fn every_response_carries_the_security_headers() {
        let (app, dir) = test_app().await;
        let data_dir = dir.path().to_str().expect("utf8 path").to_string();
        let viewer = session_for(&data_dir, Role::Viewer, "viewer1").await;
        let cookie = format!("{SESSION_COOKIE}={viewer}");

        // (method, uri, session cookie, Origin, expected status)
        let cases = [
            ("GET", "/login", None, None, StatusCode::OK),
            ("GET", "/", Some(&cookie), None, StatusCode::OK),
            (
                "GET",
                "/static/js/portal-charts.js",
                None,
                None,
                StatusCode::OK,
            ),
            (
                "GET",
                "/no-such-page",
                Some(&cookie),
                None,
                StatusCode::NOT_FOUND,
            ),
            ("GET", "/", None, None, StatusCode::SEE_OTHER),
            // A viewer opening the audit log gets `forbidden_page`'s 403.
            ("GET", "/audit", Some(&cookie), None, StatusCode::FORBIDDEN),
            (
                "POST",
                "/logout",
                Some(&cookie),
                Some("https://evil.example"),
                StatusCode::FORBIDDEN,
            ),
        ];
        for (method, uri, cookie, origin, status) in cases {
            let mut builder = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::HOST, "localhost");
            if let Some(cookie) = cookie {
                builder = builder.header(header::COOKIE, cookie);
            }
            if let Some(origin) = origin {
                builder = builder.header(header::ORIGIN, origin);
            }
            let response = app
                .clone()
                .oneshot(builder.body(Body::empty()).unwrap())
                .await
                .expect("response");
            assert_eq!(response.status(), status, "{method} {uri}");
            let headers = response.headers();
            assert_eq!(
                headers.get(header::CONTENT_SECURITY_POLICY).unwrap(),
                CONTENT_SECURITY_POLICY,
                "{method} {uri}"
            );
            assert_eq!(
                headers.get("permissions-policy").unwrap(),
                PERMISSIONS_POLICY,
                "{method} {uri}"
            );
        }
    }

    async fn test_app() -> (Router, tempfile::TempDir) {
        test_app_with(Arc::new(StateStore::new())).await
    }

    /// [`test_app`] over a caller's `store`, for a test that needs a Hub in it.
    async fn test_app_with(store: Arc<StateStore>) -> (Router, tempfile::TempDir) {
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
        let keys = KeyService::open(&data_dir, None).expect("open key service");
        let calibration = crate::calibration::CalibrationService::test_instance(
            db.clone(),
            &data_dir,
            Arc::clone(&store),
        );
        let config = PortalConfig {
            bind_addr: "127.0.0.1:0".to_string(),
            mqtt_host: "localhost".to_string(),
            mqtt_port: 1883,
            mqtt_username: None,
            mqtt_password: None,
            mqtt_client_id: "proximos-portal".into(),
            topic_prefix: "proximos".to_string(),
            data_dir,
            db_key: None,
            templates_dir: "templates".to_string(),
            static_dir: "static".to_string(),
            cookie_secure: false,
            local_hostname: None,
            trusted_proxies: Vec::new(),
            experimental_features_enabled: false,
            nodes: Default::default(),
            beacons: Default::default(),
            signal_quality: Default::default(),
            thresholds: Default::default(),
            calibration: Default::default(),
            maintenance_window: None,
        };

        let relay = CommandRelay::test_instance();
        let backfill = BackfillService::test_instance(config.data_dir.as_str());
        let alarms = AlarmService::test_instance(config.data_dir.as_str());
        let maintenance = crate::retention::Maintenance::test_instance(config.data_dir.as_str());

        (
            app(
                &config,
                store,
                auth,
                db.clone(),
                crate::durable::test_gate(&config.data_dir),
                relay,
                crate::radio::RadioControl::from_env(),
                keys,
                backfill,
                crate::beacon_history::BeaconHistoryService::test_instance(&config.data_dir),
                alarms,
                crate::pending::PendingService::open(&config.data_dir, None)
                    .expect("open pending service"),
                crate::offline::OfflineService::test_instance(db),
                calibration,
                Some(maintenance),
                Arc::new(crate::sandbox::SandboxStatus {
                    state: crate::sandbox::SandboxState::Unsupported,
                    detail: "test harness".into(),
                }),
                crate::routes::test_support::idle_shutdown(),
                // No ingest loop behind a test router, so no invites to serve.
                None,
            ),
            dir,
        )
    }

    /// The two HTTP halves of the alarm-activation pop-up, over the real
    /// router: the dashboard page must serve the `#alarm-toasts` shell
    /// (the swap target lives inside the page's sse-connect wrapper), and
    /// an authenticated `/events` response stream must carry the
    /// `alarm-toast` frame when a nudge reports an opened alarm row.
    /// `sse::tests::activated_nudge_sends_a_toast_fragment` covers the
    /// render itself; this covers routing + auth + the wire format.
    #[tokio::test]
    async fn dashboard_serves_toast_shell_and_events_streams_toast_frames() {
        use futures_util::StreamExt;

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
        let keys = KeyService::open(&data_dir, None).expect("open key service");
        let store = Arc::new(StateStore::new());
        let calibration = crate::calibration::CalibrationService::test_instance(
            db.clone(),
            &data_dir,
            Arc::clone(&store),
        );
        let config = PortalConfig {
            bind_addr: "127.0.0.1:0".to_string(),
            mqtt_host: "localhost".to_string(),
            mqtt_port: 1883,
            mqtt_username: None,
            mqtt_password: None,
            mqtt_client_id: "proximos-portal".into(),
            topic_prefix: "proximos".to_string(),
            data_dir,
            db_key: None,
            templates_dir: "templates".to_string(),
            static_dir: "static".to_string(),
            cookie_secure: false,
            local_hostname: None,
            trusted_proxies: Vec::new(),
            experimental_features_enabled: false,
            maintenance_window: None,
            nodes: Default::default(),
            beacons: Default::default(),
            signal_quality: Default::default(),
            thresholds: Default::default(),
            calibration: Default::default(),
        };
        let relay = CommandRelay::test_instance();
        let backfill = BackfillService::test_instance(config.data_dir.as_str());
        let alarms = AlarmService::test_instance(config.data_dir.as_str());
        let maintenance = crate::retention::Maintenance::test_instance(config.data_dir.as_str());

        let storage = portal_storage::Storage::open(&dir.path().join("portal.db"), None)
            .expect("open storage");
        let alarm_id = storage
            .insert_alarm_event(&portal_storage::NewAlarmEvent {
                hostname: "h1".into(),
                sensor_id: "line-0".into(),
                sensor_name: Some("Fridge".into()),
                sensor_location: None,
                alarm_type: "HIGH".into(),
                severity: "critical".into(),
                from_state: Some("NORMAL".into()),
                to_state: Some("CRITICAL".into()),
                message: None,
                value: Some(15.7),
                threshold: Some(12.0),
                triggered_at: "2026-08-18T00:00:00Z".into(),
                cleared_at: None,
                acknowledged_at: None,
                acknowledged_by_user_id: None,
                acknowledged_by_username: None,
                native_key: None,
            })
            .expect("seed alarm");

        let (token, _user) = auth
            .login(
                "admin".to_string(),
                "correct-horse-battery".to_string(),
                None,
            )
            .await
            .expect("login");
        let cookie = format!("{}={token}", crate::auth::SESSION_COOKIE);
        let nudge_tx = alarms.nudge_sender();

        let app = app(
            &config,
            store,
            auth,
            db.clone(),
            crate::durable::test_gate(&config.data_dir),
            relay,
            crate::radio::RadioControl::from_env(),
            keys,
            backfill,
            crate::beacon_history::BeaconHistoryService::test_instance(&config.data_dir),
            alarms,
            crate::pending::PendingService::open(&config.data_dir, None)
                .expect("open pending service"),
            crate::offline::OfflineService::test_instance(db),
            calibration,
            Some(maintenance),
            Arc::new(crate::sandbox::SandboxStatus {
                state: crate::sandbox::SandboxState::Unsupported,
                detail: "test harness".into(),
            }),
            crate::routes::test_support::idle_shutdown(),
            None,
        );

        // Half 1: the dashboard page carries the swap-target shell.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(
            html.contains(r#"id="alarm-toasts""#) && html.contains(r#"sse-swap="alarm-toast""#),
            "dashboard page is missing the alarm-toast shell"
        );

        // Half 2: /events streams the toast frame for an activated nudge.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/events")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let mut stream = response.into_body().into_data_stream();
        let mut collected = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while !collected.contains("event: alarm-toast") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no alarm-toast frame within 10s; stream so far: {collected}"
            );
            // The handler subscribes to the nudge broadcast only once its
            // task starts; a send before that is lost — re-send on timeout.
            let _ = nudge_tx.send(crate::alarms::AlarmNudge {
                hostname: "h1".to_string(),
                activated_ids: vec![alarm_id.clone()],
                escalated_ids: Vec::new(),
            });
            match tokio::time::timeout(std::time::Duration::from_millis(200), stream.next()).await {
                Ok(Some(Ok(chunk))) => collected.push_str(&String::from_utf8_lossy(&chunk)),
                Ok(Some(Err(err))) => panic!("body stream error: {err}"),
                Ok(None) => panic!("stream ended before an alarm-toast frame"),
                Err(_) => {}
            }
        }
        assert!(
            collected.contains("alert-error") && collected.contains("Fridge"),
            "toast frame lacks expected content: {collected}"
        );
    }

    /// #114: each group's export dialog is a `.group-export-form` — the hook
    /// the page's script binds to fetch the file instead of navigating to it
    /// — and carries, translated, the messages that script reports a failed
    /// download with. Viewer-visible, like the export itself.
    #[tokio::test]
    async fn the_group_export_dialog_carries_its_failure_messages() {
        let (state, _dir) = crate::routes::test_support::test_state().await;
        // The page draws its groups only once some Hub has reported.
        state
            .store
            .apply(crate::state::events::IngestEvent::Status {
                hostname: "h1".into(),
                status: fiber_protocol::DeviceStatus::Online,
            });
        let id = state
            .db
            .write(|s| {
                s.create_sensor_group(
                    "Freezers",
                    None,
                    &[portal_storage::NewSensorGroupMember {
                        hostname: "h1".into(),
                        sensor_line: Some(0),
                        node_key: None,
                        mac: None,
                        fields: None,
                    }],
                )
            })
            .await
            .expect("seed group");
        let token =
            crate::routes::test_support::login_as_role(&state, Role::Viewer, "viewer1").await;
        let viewer = state.auth.validate(&token).await.expect("session");

        let response = dashboard_handler(
            viewer,
            State(state.clone()),
            axum::extract::Query(DashboardQuery { error: None }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        for needle in [
            format!(r#"action="/export/group/{id}.csv" class="group-export-form "#),
            r#"data-export-failed="The export failed (HTTP {status}) and nothing was downloaded. Try again.""#.into(),
            r#"data-export-too-wide="The range can span at most 366 days.""#.into(),
            r#"data-export-unreachable="The export failed: the portal did not answer and nothing was downloaded. Try again.""#.into(),
            "data-export-busy".into(),
        ] {
            assert!(html.contains(&needle), "the dialog lacks {needle}");
        }
    }

    async fn get(app: &Router, path: &str) -> axum::http::Response<Body> {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// UX-020: the row a page draws is the row it refetches. The fragment
    /// comes back with its own trigger and URL (so it keeps listening after
    /// the swap) and with the page's filter links, not `/alarms`.
    #[tokio::test]
    async fn the_stats_fragment_refetches_itself_with_the_pages_links() {
        let (state, _dir) = crate::routes::test_support::test_state().await;
        let token =
            crate::routes::test_support::login(&state, "admin", "correct-horse-battery").await;
        let user = state.auth.validate(&token).await.expect("session");
        let body = |r: Response| async move {
            let bytes = axum::body::to_bytes(r.into_body(), 64 * 1024)
                .await
                .expect("body");
            // minijinja's HTML escaping writes `/` as `&#x2f;` in attributes.
            String::from_utf8(bytes.to_vec())
                .expect("utf8")
                .replace("&#x2f;", "/")
        };

        for (scope, oor) in [
            ("nodes", "/nodes?oor=alarming"),
            ("monitoring", "/?status=alarming"),
        ] {
            let html = body(
                stats_fragment(
                    user.clone(),
                    State(state.clone()),
                    axum::extract::Path(scope.into()),
                )
                .await,
            )
            .await;
            assert!(
                html.contains(&format!(r#"hx-get="/stats/{scope}""#)),
                "{html}"
            );
            assert!(html.contains(r#"hx-trigger="sse:page-stats""#), "{html}");
            assert!(html.contains(&format!(r#"href="{oor}""#)), "{html}");
        }
        let html = body(
            stats_fragment(
                user,
                State(state.clone()),
                axum::extract::Path("nonsense".into()),
            )
            .await,
        )
        .await;
        assert!(html.is_empty(), "{html}");
    }

    /// MR !103 review: every tab refetches the row on the same nudge, so the
    /// tabs one nudge reaches share one build. A change shows once the memo
    /// (one second, shorter than the nudge window) has expired.
    #[tokio::test]
    async fn the_stats_row_is_built_once_for_the_tabs_one_nudge_reaches() {
        use crate::state::IngestEvent;
        use fiber_protocol::payloads::status::DeviceStatus;

        let (state, _dir) = crate::routes::test_support::test_state().await;
        let token =
            crate::routes::test_support::login(&state, "admin", "correct-horse-battery").await;
        let user = state.auth.validate(&token).await.expect("session");
        let fetch = || async {
            let response = stats_fragment(
                user.clone(),
                State(state.clone()),
                axum::extract::Path("hubs".into()),
            )
            .await;
            let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .expect("body");
            String::from_utf8(bytes.to_vec()).expect("utf8")
        };

        let before = fetch().await;
        state.store.apply(IngestEvent::Status {
            hostname: "hub-a".into(),
            status: DeviceStatus::Online,
        });
        assert_eq!(fetch().await, before, "a second tab gets the same build");
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert_ne!(
            fetch().await,
            before,
            "after the memo expires, the Hub counts"
        );
    }

    #[tokio::test]
    async fn anonymous_protected_routes_redirect_to_login() {
        let (app, _dir) = test_app().await;

        // A GET keeps its target for after the login (UX-027); `/` is where
        // a login lands anyway.
        for (path, want) in [
            ("/", "/login"),
            ("/events", "/login?next=%2Fevents"),
            ("/audit", "/login?next=%2Faudit"),
            ("/hubs", "/login?next=%2Fhubs"),
            ("/stats/nodes?x=1", "/login?next=%2Fstats%2Fnodes%3Fx%3D1"),
            // calibration.cs.md UI amendment 2026-10-14: the modal's card
            // is viewer+, like the page it opens on.
            (
                "/hubs/hub1/sensors/0/calibrations/card",
                "/login?next=%2Fhubs%2Fhub1%2Fsensors%2F0%2Fcalibrations%2Fcard",
            ),
            (
                "/nodes/70b3d5e050011234/calibrations/card",
                "/login?next=%2Fnodes%2F70b3d5e050011234%2Fcalibrations%2Fcard",
            ),
            (
                "/beacons/AA:BB:CC:DD:EE:FF/calibrations/card",
                "/login?next=%2Fbeacons%2FAA%3ABB%3ACC%3ADD%3AEE%3AFF%2Fcalibrations%2Fcard",
            ),
        ] {
            let response = get(&app, path).await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER, "path {path}");
            assert_eq!(
                response.headers().get(header::LOCATION).unwrap(),
                want,
                "path {path}"
            );
        }
    }

    /// M4-D role gate: a viewer (not manager/admin) hitting a system-command
    /// route gets 403, never reaching the relay. Reopens a second
    /// `AuthService` connection onto the same on-disk db `test_app` already
    /// seeded with a superuser — same "another SQLite connection under WAL
    /// is fine" pattern the binary itself uses for its own service structs.
    #[tokio::test]
    async fn viewer_gets_403_on_hub_system_restart() {
        let (app, dir) = test_app().await;
        let data_dir = dir.path().to_str().expect("utf8 path").to_string();

        let auth = AuthService::open(&data_dir, None, crate::durable::test_gate(&data_dir))
            .expect("reopen auth service");
        let (_admin_token, admin_current) = auth
            .login(
                "admin".to_string(),
                "correct-horse-battery".to_string(),
                None,
            )
            .await
            .expect("login admin");
        auth.create_user(
            &admin_current,
            NewUserInput {
                username: "viewer1".to_string(),
                password: "viewerpass123".to_string(),
                full_name: None,
                job_title: None,
                user_role: Role::Viewer,
            },
        )
        .await
        .expect("create viewer user");
        let (viewer_token, _) = auth
            .login("viewer1".to_string(), "viewerpass123".to_string(), None)
            .await
            .expect("login viewer");

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/hubs/hub1/system/restart")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={viewer_token}"))
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("reason="))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// UX-037: a viewer opening an admin page gets the error page inside the
    /// layout (status kept), and `/settings` with no section a real 404 page.
    #[tokio::test]
    async fn a_refused_or_missing_page_keeps_the_layout() {
        let (app, dir) = test_app().await;
        let data_dir = dir.path().to_str().expect("utf8 path").to_string();
        let auth = AuthService::open(&data_dir, None, crate::durable::test_gate(&data_dir))
            .expect("reopen auth service");
        let (_, admin) = auth
            .login(
                "admin".to_string(),
                "correct-horse-battery".to_string(),
                None,
            )
            .await
            .expect("login admin");
        auth.create_user(
            &admin,
            NewUserInput {
                username: "viewer1".to_string(),
                password: "viewerpass123".to_string(),
                full_name: None,
                job_title: None,
                user_role: Role::Viewer,
            },
        )
        .await
        .expect("create viewer");
        let (viewer, _) = auth
            .login("viewer1".to_string(), "viewerpass123".to_string(), None)
            .await
            .expect("login viewer");

        for (uri, status, words) in [
            ("/settings/users", StatusCode::FORBIDDEN, "Access denied"),
            ("/settings", StatusCode::NOT_FOUND, "Page not found"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(header::COOKIE, format!("{SESSION_COOKIE}={viewer}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{uri}");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            let body = String::from_utf8(bytes.to_vec()).expect("utf8");
            assert!(
                body.contains("<html") && body.contains(words),
                "{uri}: {body}"
            );
            assert!(body.contains(r#"href="/""#), "{uri}: a way back");
        }
    }

    /// The minimum role a mutating route requires. `Public` and `SelfService`
    /// are not rungs of the ladder — they are the two deliberate exemptions,
    /// spelled out rather than omitted so the completeness sweep below cannot
    /// pass by simply leaving a route out.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Gate {
        /// Pre-auth by definition (setup/login/logout), or a public preference.
        Public,
        /// Any authenticated operator, acting on their own account only.
        SelfService,
        Manager,
        Admin,
    }

    /// Every mutating route in the process and the role it demands
    /// (plan/portal/devices.backend.md §3, WP0.1).
    ///
    /// This table is a specification, not a summary of the code: it is
    /// asserted against real requests below, and a route missing from it
    /// fails `every_mutating_route_declares_a_role`. Registering a `post`
    /// route without deciding its gate is therefore a test failure — which is
    /// the point. The one route that had been reachable by any signed-in
    /// viewer (`POST /device/pair`, while its sibling `unpair` was admin-only)
    /// got that way because nobody had to choose.
    const MUTATING_ROUTES: &[(&str, Gate, &str)] = &[
        // -- pre-auth, or a preference with nothing behind it -------------
        ("/setup", Gate::Public, ""),
        ("/login", Gate::Public, ""),
        ("/logout", Gate::Public, ""),
        ("/lang", Gate::Public, "lang=en"),
        // -- self-service: the actor's own account only --------------------
        ("/settings/password", Gate::SelfService, ""),
        (
            "/monitoring/view-mode",
            Gate::SelfService,
            "mode=table-detailed",
        ),
        // -- user administration -------------------------------------------
        ("/settings/users", Gate::Admin, ""),
        ("/settings/users/1/edit", Gate::Admin, ""),
        ("/settings/users/1/enable", Gate::Admin, ""),
        ("/settings/users/1/disable", Gate::Admin, ""),
        ("/settings/users/1/delete", Gate::Admin, ""),
        ("/settings/users/1/reset-password", Gate::Admin, ""),
        ("/settings/users/1/edit", Gate::Admin, ""),
        // -- the Hub itself (devices.hub.md §8.1) -----------------------------
        // Restart is admin, tightened from the shipped manager+ so the gate
        // agrees with the tab it lives on (§3.3).
        // Naming a Hub is manager+ (§7): it is operator metadata, not the
        // device identity the admin tier guards.
        ("/hubs/hub1/meta", Gate::Manager, "name=&description="),
        ("/hubs/hub1/pair", Gate::Admin, ""),
        // External gateways: backend preparation, no page links them yet (2026-09-26).
        ("/hubs/hub1/radio/gateways", Gate::Admin, ""),
        ("/hubs/hub1/radio/gateways/remove", Gate::Admin, ""),
        // "Přesunout všechny Nody na <rádio>" (R6): one `node-move` per Node, admin.
        (
            "/hubs/hub1/radio/move-nodes",
            Gate::Admin,
            "to=p2p&confirm_hostname=nope",
        ),
        // „Aktualizovat firmware NB“ (northbridge_update.md §3.4, amended 2026-10-09): the
        // signed `northbridge-update`, admin like `radio-set-mode`.
        ("/hubs/hub1/radio/northbridge-update", Gate::Admin, ""),
        // „Aktualizovat firmware SB“ (southbridge_update.md §3.7): the signed
        // `southbridge-update`, admin like `northbridge-update`.
        ("/hubs/hub1/system/southbridge-update", Gate::Admin, ""),
        ("/hubs/hub1/unpair", Gate::Admin, ""),
        // C5, devices.hub.md §8.1: Add hub, Retry, the audited reveal of the
        // join bundle and Remove are admin — they mint, rotate and revoke a
        // broker credential.
        ("/hubs/hub1/cluster/clients", Gate::Admin, ""),
        (
            "/hubs/hub1/cluster/clients/proximos-1a2b3c4d/retry",
            Gate::Admin,
            "",
        ),
        (
            "/hubs/hub1/cluster/clients/proximos-1a2b3c4d/reveal",
            Gate::Admin,
            "token=x",
        ),
        ("/hubs/hub1/cluster/remove", Gate::Admin, "reason=x"),
        ("/hubs/hub1/cluster/gateway", Gate::Admin, "gateway_eui=x"),
        ("/hubs/hub1/system/restart", Gate::Admin, "reason="),
        ("/hubs/hub1/system/power-off", Gate::Admin, "reason="),
        ("/hubs/hub1/system/factory-reset", Gate::Admin, "reason="),
        // "Změnit rádio Hubu" (§3.4, UI-24 as amended; node_registry.md §11).
        (
            "/hubs/hub1/system/radio-mode",
            Gate::Admin,
            "mode=p2p&confirm_hostname=hub1",
        ),
        // -- wired probes -----------------------------------------------------
        ("/hubs/hub1/sensors/1/threshold", Gate::Manager, ""),
        ("/hubs/hub1/sensors/1/name", Gate::Manager, "value=x"),
        ("/hubs/hub1/sensors/1/meta", Gate::Manager, "description=x"),
        ("/hubs/hub1/sensors/1/location", Gate::Manager, "value=x"),
        (
            "/hubs/hub1/system/info-interval",
            Gate::Admin,
            "interval_seconds=60",
        ),
        (
            "/hubs/hub1/system/read-interval",
            Gate::Admin,
            "sample_interval_s=5",
        ),
        // -- the Unit tab: display, buzzer, label, LCD sensor picker -------------
        // Manager+, one tier below the power actions beside them
        // (devices.hub.md §3.3/§7, settling display-config.md's OQ 2).
        (
            "/hubs/hub1/display/buzzer-volume",
            Gate::Manager,
            "value=50",
        ),
        (
            "/hubs/hub1/display/screen-brightness",
            Gate::Manager,
            "value=80",
        ),
        (
            "/hubs/hub1/display/led-brightness",
            Gate::Manager,
            "value=75",
        ),
        (
            "/hubs/hub1/display/screen-timeout",
            Gate::Manager,
            "value=3600",
        ),
        (
            "/hubs/hub1/display/device-label",
            Gate::Manager,
            "value=Ward+3",
        ),
        (
            "/hubs/hub1/display/sensors",
            Gate::Manager,
            "sensors=ds18b20%3A0",
        ),
        // -- Nodes -------------------------------------------------------------
        (
            "/nodes/70b3d5e050011234/thresholds/temperature",
            Gate::Manager,
            "",
        ),
        (
            "/nodes/70b3d5e050011234/sensors/temperature",
            Gate::Manager,
            "enabled=false",
        ),
        (
            "/nodes/70b3d5e050011234/config",
            Gate::Manager,
            // The dotted key the form actually posts. An undotted
            // `interval_report` is rejected by the handler's own key check,
            // so the sweep used to see its 403/redirect for the wrong reason
            // — it proved the validation gate, not the role gate (§21
            // review, item 15).
            "key=application.interval_report&value=60",
        ),
        ("/nodes/70b3d5e050011234/refresh-config", Gate::Manager, ""),
        // B4's draft: any manager may send or discard the shared draft,
        // whoever typed the values into it (decision 2026-09-07).
        ("/nodes/70b3d5e050011234/pending/send", Gate::Manager, ""),
        ("/nodes/70b3d5e050011234/pending/discard", Gate::Manager, ""),
        ("/nodes/70b3d5e050011234/limits/resync", Gate::Manager, ""),
        // The Device card's GetInfo + GetSettings (nodes_phase1.cs.md WP7): manager, like the
        // config refresh.
        ("/nodes/70b3d5e050011234/read-device", Gate::Manager, ""),
        ("/nodes/70b3d5e050011234/dev-status", Gate::Manager, ""),
        // The service card (devices.node.md §3.6b, nodes_phase1.cs.md WP6): admin, all three.
        (
            "/nodes/70b3d5e050011234/service/force-send",
            Gate::Admin,
            "",
        ),
        (
            "/nodes/70b3d5e050011234/service/send-msg",
            Gate::Admin,
            "payload=%7B%7D",
        ),
        (
            "/nodes/70b3d5e050011234/service/reboot",
            Gate::Admin,
            "confirm=nope",
        ),
        // B4's lifecycle (devices.node.md §5/§6/§3.7): registration and
        // removal are fleet work and so manager; a key reveal is admin,
        // like the service card it sits beside.
        (
            "/nodes",
            Gate::Manager,
            "serial=1&dev_eui=70b3d5e050011234&profile=STICKER\
             &app_key=00000000000000000000000000000000&transport=lorawan",
        ),
        // `remove_lorawan_sticker` is `destructive` — same call as the restart above.
        (
            "/nodes/70b3d5e050011234/remove",
            Gate::Admin,
            "confirm=nope",
        ),
        // "Změnit rádio" (node_registry.md §6): `node-move` is `admin`, like removal.
        (
            "/nodes/70b3d5e050011234/move",
            Gate::Admin,
            "to=p2p&confirm=nope",
        ),
        ("/nodes/70b3d5e050011234/keys/reveal", Gate::Admin, ""),
        (
            "/nodes/70b3d5e050011234/meta",
            Gate::Manager,
            "description=cold+room&offline_k=",
        ),
        // The Device configuration card: drafts, saves and sends (ui_components.md §6).
        (
            "/nodes/70b3d5e050011234/device-config",
            Gate::Manager,
            "offline_k=&history=on",
        ),
        // The History switch drafts a config change and sets the automatic download.
        (
            "/nodes/70b3d5e050011234/history",
            Gate::Manager,
            "enabled=true",
        ),
        (
            "/nodes/70b3d5e050011234/backfill",
            Gate::Manager,
            "from=&to=",
        ),
        (
            "/nodes/70b3d5e050011234/backfill/1/cancel",
            Gate::Manager,
            "",
        ),
        // calibration.cs.md §2: uploading a certificate and moving its date
        // are manager work on either device kind; the download is a GET.
        // The upload's empty urlencoded body fails the multipart extractor
        // for a manager, which is past the role gate — all the sweep asks.
        ("/nodes/70b3d5e050011234/calibrations", Gate::Manager, ""),
        (
            "/nodes/70b3d5e050011234/calibrations/date",
            Gate::Manager,
            "valid_until=2026-12-31",
        ),
        ("/beacons/AA:BB:CC:DD:EE:FF/calibrations", Gate::Manager, ""),
        (
            "/beacons/AA:BB:CC:DD:EE:FF/calibrations/date",
            Gate::Manager,
            "valid_until=2026-12-31",
        ),
        // Amendment 2026-10-13: deleting a certificate is admin only.
        (
            "/nodes/70b3d5e050011234/calibrations/0123456789abcdef0123456789abcdef/delete",
            Gate::Admin,
            "message=x",
        ),
        (
            "/beacons/AA:BB:CC:DD:EE:FF/calibrations/0123456789abcdef0123456789abcdef/delete",
            Gate::Admin,
            "message=x",
        ),
        // Amendment 2026-10-14: a Hub probe's card, the same gates.
        ("/hubs/hub1/sensors/0/calibrations", Gate::Manager, ""),
        (
            "/hubs/hub1/sensors/0/calibrations/date",
            Gate::Manager,
            "valid_until=2026-12-31",
        ),
        (
            "/hubs/hub1/sensors/0/calibrations/0123456789abcdef0123456789abcdef/delete",
            Gate::Admin,
            "message=x",
        ),
        // -- Beacons -------------------------------------------------------------
        // devices.beacon.md §7 (line 342) reads "manager"; B0 shipped these
        // two as admin, so a manager — the role the spec designates for
        // fleet work — had to escalate for a routine registration (§19, F10).
        // B6: the form posts SN + MAC + name, and the sweep's body has to
        // carry a serial or the handler refuses it for the wrong reason —
        // it would prove the validation gate, not the role gate (§21 review,
        // item 15, on the Node side).
        (
            "/beacons/add",
            Gate::Manager,
            "serial=6845019001&mac=AA%3ABB%3ACC%3ADD%3AEE%3AFF&name=x",
        ),
        // `beacon-remove` is `destructive` — same call again.
        ("/beacons/AA:BB:CC:DD:EE:FF/remove", Gate::Admin, ""),
        // B6's two new commands (devices.beacon.md §3.5, §3.8): both are
        // fleet work, both manager, both audited.
        ("/beacons/AA:BB:CC:DD:EE:FF/reconfigure", Gate::Manager, ""),
        ("/beacons/AA:BB:CC:DD:EE:FF/history", Gate::Manager, ""),
        (
            "/beacons/AA:BB:CC:DD:EE:FF/history/bh-1/cancel",
            Gate::Manager,
            "",
        ),
        (
            "/beacons/AA:BB:CC:DD:EE:FF/history-auto",
            Gate::Manager,
            "enabled=true",
        ),
        (
            "/beacons/AA:BB:CC:DD:EE:FF/meta",
            Gate::Manager,
            "description=freezer&offline_k=",
        ),
        (
            "/beacons/AA:BB:CC:DD:EE:FF/device-config",
            Gate::Manager,
            "offline_k=&history=on",
        ),
        (
            "/beacons/AA:BB:CC:DD:EE:FF/thresholds/temperature",
            Gate::Manager,
            "",
        ),
        (
            "/beacons/AA:BB:CC:DD:EE:FF/sensors/temperature",
            Gate::Manager,
            "enabled=false",
        ),
        // -- alarms ----------------------------------------------------------------
        ("/alarms/acknowledge", Gate::Manager, ""),
        ("/alarms/1/acknowledge", Gate::Manager, ""),
        ("/alarms/1/notes", Gate::Manager, "note=x"),
        // -- monitoring groups --------------------------------------------------------
        ("/monitoring/groups", Gate::Manager, "name=g"),
        ("/monitoring/groups/1", Gate::Manager, "name=g"),
        ("/monitoring/groups/1/delete", Gate::Manager, ""),
        ("/monitoring/groups/1/move", Gate::Manager, "direction=up"),
        // -- MB3's manual probe events --------------------------------------------------
        // Concrete paths with `probe:0`, a token that parses but resolves to
        // nothing in the test app: both handlers must answer a redirect
        // rather than a 500 for it, which is exactly what the sweep proves
        // by POSTing here as a manager.
        (
            "/monitoring/probe/probe:0/events",
            Gate::Manager,
            "at=2026-09-09T10:00&text=x&range=24h",
        ),
        (
            "/monitoring/probe/probe:0/events/1/delete",
            Gate::Manager,
            "range=24h",
        ),
        // -- notifications ---------------------------------------------------------------
        ("/settings/notifications", Gate::Manager, ""),
        ("/settings/notifications/test-email", Gate::Admin, ""),
        ("/settings/notifications/test-webhook", Gate::Admin, ""),
        // -- maintenance --------------------------------------------------------------------
        ("/settings/maintenance/vacuum", Gate::Admin, ""),
        (
            "/settings/maintenance/timezone",
            Gate::Admin,
            "timezone=Europe%2FPrague",
        ),
    ];

    async fn post_as(app: &Router, path: &str, token: Option<&str>, body: &str) -> StatusCode {
        let mut builder = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(token) = token {
            builder = builder.header(header::COOKIE, format!("{SESSION_COOKIE}={token}"));
        }
        app.clone()
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// Creates `username` with `role` against the temp dir `test_app` handed
    /// back, and returns their session token.
    async fn session_for(data_dir: &str, role: Role, username: &str) -> String {
        let auth = AuthService::open(data_dir, None, crate::durable::test_gate(data_dir))
            .expect("reopen auth service");
        let (_token, admin) = auth
            .login(
                "admin".to_string(),
                "correct-horse-battery".to_string(),
                None,
            )
            .await
            .expect("login admin");
        auth.create_user(
            &admin,
            NewUserInput {
                username: username.to_string(),
                password: "role-test-pass-123".to_string(),
                full_name: None,
                job_title: None,
                user_role: role,
            },
        )
        .await
        .expect("create user");
        auth.login(username.to_string(), "role-test-pass-123".to_string(), None)
            .await
            .expect("login")
            .0
    }

    /// WP0.1's acceptance condition: **every mutating route rejects an
    /// insufficient role server-side**, proven by real requests through the
    /// whole app — not by inspecting a template to see whether a button was
    /// drawn.
    ///
    /// The assertion is one-sided on purpose. "Forbidden" is exact; "allowed"
    /// is only ever `!= 403`, because a permitted request runs the real
    /// handler and lands on whatever that handler makes of a test fixture (a
    /// redirect, a 404 for a Node that does not exist, a 422 for an empty
    /// backfill window). Pinning those would be testing the handlers, not the
    /// gate.
    #[tokio::test]
    async fn every_mutating_route_enforces_its_role_server_side() {
        let (app, dir) = test_app().await;
        let data_dir = dir.path().to_str().expect("utf8 path").to_string();

        let viewer = session_for(&data_dir, Role::Viewer, "viewer1").await;
        let manager = session_for(&data_dir, Role::Manager, "manager1").await;

        for (path, gate, body) in MUTATING_ROUTES {
            match gate {
                Gate::Public => continue,
                Gate::SelfService => {
                    assert_ne!(
                        post_as(&app, path, Some(&viewer), body).await,
                        StatusCode::FORBIDDEN,
                        "viewer must be able to POST {path} (self-service)"
                    );
                }
                Gate::Manager => {
                    assert_eq!(
                        post_as(&app, path, Some(&viewer), body).await,
                        StatusCode::FORBIDDEN,
                        "viewer must NOT be able to POST {path}"
                    );
                    assert_ne!(
                        post_as(&app, path, Some(&manager), body).await,
                        StatusCode::FORBIDDEN,
                        "manager must be able to POST {path}"
                    );
                }
                Gate::Admin => {
                    assert_eq!(
                        post_as(&app, path, Some(&viewer), body).await,
                        StatusCode::FORBIDDEN,
                        "viewer must NOT be able to POST {path}"
                    );
                    assert_eq!(
                        post_as(&app, path, Some(&manager), body).await,
                        StatusCode::FORBIDDEN,
                        "manager must NOT be able to POST {path}"
                    );
                }
            }
        }
    }

    /// The Hub as a browser addresses it: `Host` as nginx forwards it, and
    /// the origin a page served from it posts from.
    const HUB_HOST: &str = "proximos-a1b2c3d4.local";
    const HUB_ORIGIN: &str = "https://proximos-a1b2c3d4.local";

    /// The ways a POST gives away that a foreign page sent it, one per rung
    /// of `auth::origin_guard`: a foreign `Origin`; with none,
    /// `Sec-Fetch-Site: cross-site`; with neither, a foreign `Referer`.
    const FOREIGN_PAGE: [&[(&str, &str)]; 3] = [
        &[("origin", "https://evil.example")],
        &[("sec-fetch-site", "cross-site")],
        &[("referer", "https://evil.example/csrf.html")],
    ];

    /// A form POST to `path` addressed to the Hub, with `headers` on top and
    /// the session cookie when `token` is given. It carries `ConnectInfo`
    /// like a served request, so `/login` reaches its handler.
    fn hub_post(
        path: &str,
        token: Option<&str>,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::HOST, HUB_HOST)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                1,
            ))));
        if let Some(token) = token {
            builder = builder.header(header::COOKIE, format!("{SESSION_COOKIE}={token}"));
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    /// `true` if `response` is `auth::origin_guard`'s refusal, not a 403 a
    /// role gate further in rendered.
    async fn refused_by_origin_guard(response: axum::http::Response<Body>) -> bool {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        status == StatusCode::FORBIDDEN && bytes.as_ref() == b"cross-origin request blocked"
    }

    /// GitLab #105, plan/portal/design.md §4 (amended 2026-10-10): no
    /// mutating route adds a CSRF defence of its own. They all sit behind
    /// `auth::origin_guard`, the public ones (`/setup`, `/login`, `/logout`,
    /// `/lang`) included, and so do the Hub and gateway routes, as §8.6 of
    /// devices.hub.md and devices.external.md says. A POST to each that a
    /// foreign page could have sent is refused before its handler runs, even
    /// with a valid admin session. The same POST from the Hub's own origin
    /// gets past the guard. That half goes out anonymously on purpose: the
    /// guard runs first either way, and an admin session would run every
    /// admin handler (test e-mail, vacuum, disabling a user) as a side effect.
    #[tokio::test]
    async fn every_mutating_route_refuses_a_cross_origin_post() {
        let (app, dir) = test_app().await;
        let data_dir = dir.path().to_str().expect("utf8 path").to_string();
        let (admin, _) = AuthService::open(&data_dir, None, crate::durable::test_gate(&data_dir))
            .expect("reopen auth service")
            .login(
                "admin".to_string(),
                "correct-horse-battery".to_string(),
                None,
            )
            .await
            .expect("login admin");

        let public = MUTATING_ROUTES
            .iter()
            .filter(|(_, gate, _)| *gate == Gate::Public)
            .count();
        assert_eq!(public, 4, "setup, login, logout and lang");

        for (path, _, body) in MUTATING_ROUTES {
            for foreign in FOREIGN_PAGE {
                let response = app
                    .clone()
                    .oneshot(hub_post(path, Some(&admin), foreign, body))
                    .await
                    .unwrap();
                assert!(
                    refused_by_origin_guard(response).await,
                    "POST {path} with {foreign:?}"
                );
            }
            let response = app
                .clone()
                .oneshot(hub_post(path, None, &[("origin", HUB_ORIGIN)], body))
                .await
                .unwrap();
            assert!(
                !refused_by_origin_guard(response).await,
                "same-origin POST {path}"
            );
        }
    }

    /// GitLab #105: a login CSRF, a foreign page posting the attacker's
    /// credentials so the victim's browser ends up signed in as the
    /// attacker, is refused whichever header gives the foreign page away, and
    /// no session cookie is issued. A login from the Hub's own login page
    /// still signs in, and so does one from a client that sends none of the
    /// three headers (curl, the desktop client).
    #[tokio::test]
    async fn a_login_csrf_is_refused_and_a_real_login_still_signs_in() {
        let (app, _dir) = test_app().await;
        let credentials = "username=admin&password=correct-horse-battery";
        let sets_session = |response: &axum::http::Response<Body>| {
            response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .any(|v| v.starts_with(SESSION_COOKIE))
        };

        for foreign in FOREIGN_PAGE {
            let response = app
                .clone()
                .oneshot(hub_post("/login", None, foreign, credentials))
                .await
                .unwrap();
            assert!(!sets_session(&response), "session issued for {foreign:?}");
            assert!(
                refused_by_origin_guard(response).await,
                "login with {foreign:?}"
            );
        }

        for own in [
            &[("origin", HUB_ORIGIN)][..],
            &[("sec-fetch-site", "same-origin")],
            &[("referer", "https://proximos-a1b2c3d4.local/login")],
            &[],
        ] {
            let response = app
                .clone()
                .oneshot(hub_post("/login", None, own, credentials))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SEE_OTHER, "{own:?}");
            assert!(sets_session(&response), "no session for {own:?}");
        }
    }

    /// An anonymous POST is redirected to the login page, never silently
    /// executed — on the self-service routes too.
    #[tokio::test]
    async fn anonymous_mutations_redirect_to_login() {
        let (app, _dir) = test_app().await;

        for (path, gate, body) in MUTATING_ROUTES {
            if *gate == Gate::Public {
                continue;
            }
            assert_eq!(
                post_as(&app, path, None, body).await,
                StatusCode::SEE_OTHER,
                "anonymous POST {path} must redirect to /login"
            );
        }
    }

    /// The table above is a specification only if it is complete. Parses the
    /// `post` registrations out of every route module and asserts each one is
    /// declared, so a new mutating route cannot ship without someone choosing
    /// its gate. Same source-derived approach as `every_nav_link_resolves`,
    /// and it fails the same way if the parser stops matching.
    #[test]
    fn every_mutating_route_declares_a_role() {
        let routes_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routes");
        let mut registered: Vec<String> = Vec::new();

        for entry in std::fs::read_dir(&routes_dir).expect("read routes dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("read route module");
            // Stop at the module's tests. Routers are only ever built above
            // them, while the harnesses below are full of `post(` helpers —
            // and since the last registration's span runs to the end of the
            // file, one of those would hand the module's final route a
            // phantom `post`. That bit `/export/group/{id_csv}`, a `get`
            // route that happens to be registered last, the moment
            // group_routes gained oneshot tests.
            let src = src.split("\n#[cfg(test)]").next().unwrap_or(&src);

            // Each registration is `.route(<path literal>, <methods>)`, and
            // rustfmt wraps the longer ones so the path sits on its own line
            // — hence matching `.route(` rather than `.route("`, then
            // skipping whitespace to the literal. Getting that wrong makes
            // one registration's span swallow the next, which is exactly how
            // a `get`-only route acquires a phantom `post`.
            //
            // A registration's span runs to the start of the next one (or the
            // end of the file); `post(` anywhere inside it means this path
            // mutates.
            let starts: Vec<usize> = src.match_indices(".route(").map(|(i, _)| i).collect();
            for (n, &start) in starts.iter().enumerate() {
                let end = starts.get(n + 1).copied().unwrap_or(src.len());
                let span = &src[start..end];
                let Some(route) = span
                    .strip_prefix(".route(")
                    .map(|rest| rest.trim_start())
                    .and_then(|rest| rest.strip_prefix('"'))
                    .and_then(|rest| rest.split_once('"'))
                    .map(|(route, _)| route)
                else {
                    continue;
                };
                if span.contains("post(") {
                    registered.push(route.to_string());
                }
            }
        }
        registered.sort();
        registered.dedup();

        // Named low-water mark, so a parser that quietly stops matching fails
        // here instead of passing on an empty sweep.
        assert!(
            registered.len() >= 30,
            "route parser found too little: {registered:?}"
        );
        assert!(
            registered.iter().any(|r| r == "/hubs/{hub}/pair"),
            "route parser missed /hubs/{{hub}}/pair: {registered:?}"
        );

        // The table stores concrete paths; the router stores templates with
        // `{param}` placeholders. Compare segment-wise, with a placeholder
        // matching anything.
        let matches = |template: &str, concrete: &str| -> bool {
            let t: Vec<&str> = template.split('/').collect();
            let c: Vec<&str> = concrete.split('/').collect();
            t.len() == c.len() && t.iter().zip(&c).all(|(t, c)| t.starts_with('{') || t == c)
        };

        let missing: Vec<&String> = registered
            .iter()
            .filter(|r| !MUTATING_ROUTES.iter().any(|(d, _, _)| matches(r, d)))
            .collect();
        assert!(
            missing.is_empty(),
            "mutating routes with no role declared in MUTATING_ROUTES: {missing:?}"
        );
    }

    /// GitLab #104: public, but an anonymous client learns nothing beyond
    /// liveness — no build version, no sandbox state or Landlock error text.
    #[tokio::test]
    async fn health_is_public() {
        let (app, _dir) = test_app().await;
        let response = get(&app, "/health").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body, serde_json::json!({ "status": "healthy" }));
    }

    /// A signed-in session of any role still gets the version and the sandbox
    /// verdict.
    #[tokio::test]
    async fn health_shows_details_to_a_session() {
        let (app, dir) = test_app().await;
        let data_dir = dir.path().to_str().expect("utf8 path").to_string();
        let viewer = session_for(&data_dir, Role::Viewer, "viewer1").await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/health")
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={viewer}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["status"], "healthy");
        assert_eq!(body["version"], crate::version());
        assert!(
            body["sandbox"]["state"].is_string(),
            "no sandbox state: {body}"
        );
    }

    /// The CA download is public (plan/control/cluster.md §6): an anonymous
    /// request is answered — 404 on a host with no `/data/tls/ca.crt`, the
    /// PEM on a Hub — never sent to `/login`.
    #[tokio::test]
    async fn connection_ca_is_public() {
        let (app, _dir) = test_app().await;
        let response = get(&app, "/connection/ca").await;
        assert!(
            matches!(response.status(), StatusCode::OK | StatusCode::NOT_FOUND),
            "{:?}",
            response.status()
        );
        assert!(response.headers().get(header::LOCATION).is_none());
    }

    /// ...and it is answered before setup too: a Client fetching a fresh
    /// Server's CA must get the certificate, not the `/setup` redirect, while
    /// every other page still gets the redirect.
    #[tokio::test]
    async fn connection_ca_is_exempt_from_the_setup_gate() {
        let (mut state, _dir) = crate::routes::test_support::test_state().await;
        let fresh = tempfile::tempdir().expect("tempdir");
        let data_dir = fresh.path().to_str().expect("utf8 path").to_string();
        state.auth = AuthService::open(&data_dir, None, crate::durable::test_gate(&data_dir))
            .expect("open auth service");
        assert!(
            state.auth.setup_required().await,
            "precondition: no superuser"
        );

        let app = Router::new()
            .merge(routes::connection_routes::router())
            .route("/elsewhere", axum::routing::get(|| async { "page" }))
            .layer(middleware::from_fn_with_state(state.clone(), setup_gate))
            .with_state(state);

        let ca = get(&app, "/connection/ca").await;
        assert!(
            matches!(ca.status(), StatusCode::OK | StatusCode::NOT_FOUND),
            "{:?}",
            ca.status()
        );
        assert!(ca.headers().get(header::LOCATION).is_none());

        let page = get(&app, "/elsewhere").await;
        assert_eq!(page.headers().get(header::LOCATION).unwrap(), "/setup");
    }

    #[tokio::test]
    async fn login_page_is_public() {
        let (app, _dir) = test_app().await;
        let response = get(&app, "/login").await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn logout_is_harmless_when_anonymous() {
        let (app, _dir) = test_app().await;
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/logout")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/login");
    }

    /// GitLab #109: after `/setup` the Hub this portal runs on is not paired, and the dashboard
    /// said nothing while every signed command (the first Beacon registration included) would
    /// fail. It says so now — about the local Hub only, to every role, with the way to the
    /// pairing card for manager+ — and stops once the Hub is paired, under whichever spelling
    /// of its hostname the pairing was stored (NT-4).
    #[tokio::test]
    async fn the_dashboard_says_when_the_hub_it_runs_on_is_not_paired() {
        use crate::routes::test_support::{login, login_as_role, test_state};
        use crate::state::IngestEvent;
        use fiber_protocol::payloads::status::DeviceStatus;

        let (state, _dir) = test_state().await;
        // `test_state`'s portal believes it runs on "PORTAL-TEST-HOST"; the Hub reports in
        // lowercase, as Hubs do since decision #21. A second Hub is not this portal's own.
        for hostname in ["portal-test-host", "other-hub"] {
            state.store.apply(IngestEvent::Status {
                hostname: hostname.into(),
                status: DeviceStatus::Online,
            });
        }
        let admin = login(&state, "admin", "correct-horse-battery").await;
        let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
        let app = Router::new()
            .route("/", axum::routing::get(dashboard_handler))
            .with_state(state.clone());
        let page = |token: String| {
            let app = app.clone();
            async move {
                let response = app
                    .oneshot(
                        Request::builder()
                            .uri("/")
                            .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                String::from_utf8(bytes.to_vec()).unwrap()
            }
        };
        let banner = r#"data-unpaired-hub="portal-test-host""#;
        // #109, dodatek 2026-10-13: an admin pairs from the banner itself.
        let link = r#"data-pair-form="portal-test-host""#;

        let html = page(admin.clone()).await;
        assert!(html.contains(banner), "{html}");
        assert!(html.contains(link), "{html}");
        assert!(
            !html.contains(r#"data-unpaired-hub="other-hub""#),
            "only the Hub this portal runs on"
        );
        let html = page(viewer).await;
        assert!(html.contains(banner), "{html}");
        assert!(!html.contains(link), "pairing is not a viewer's to do");

        state
            .keys
            .store_pairing(
                "PORTAL-TEST-HOST",
                Some("test-ca".into()),
                "00".repeat(32),
                [7u8; 32],
                None,
                None,
            )
            .await
            .expect("store pairing");
        let html = page(admin).await;
        assert!(!html.contains("data-unpaired-hub"), "{html}");
    }
}
