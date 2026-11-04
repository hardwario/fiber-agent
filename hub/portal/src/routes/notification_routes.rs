//! `GET /settings/notifications` + the one config POST + the two htmx
//! test-send POSTs (M6-F). Context contract:
//! `templates/pages/notifications.html` header comment (BINDING — form
//! field names, `*_set` secret flags, `?ok=`/`?err=` flash, `?hoffset=`
//! history paging, `#command-status` test-button target).
//!
//! Role gates: the page and its save are manager-or-admin (the
//! `RequireManager` extractor, nav.html's visibility rule enforced
//! server-side). The save carries every section of the page, so the
//! admin-only half (credentials, webhook URL, system thresholds) is
//! applied only for `is_admin()` principals and silently skipped for a
//! manager — whose page never renders those fields in the first place.
//! The test-send POSTs stay admin-only (`RequireAdmin`).
//!
//! Storage access goes through the request path's `Db` (`AppState::db`),
//! config upserts included. Secrets (SendGrid API key, SMTP password,
//! webhook headers) are sealed with `AppState::sealer` and follow the template's
//! keep-on-empty contract: an empty input means "leave the stored blob
//! unchanged", never "clear it". Responses only ever carry `*_set` flags —
//! a secret is write-only once saved.
//!
//! The two test POSTs do real network I/O (SMTP/SendGrid/webhook) on an
//! operator click — acceptable by design (that's the point of a test
//! button); each writes a `report_history` row (`event_type: "test"`) and
//! renders `partials/command_status.html` like the signed-command routes,
//! minus any request-id/handshake continuity (the template header blesses
//! this stretch).

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use minijinja::context;
use portal_storage::{
    DeviceReportConfig, NewReportHistory, NotifyGlobalConfig, ReportHistoryFilter,
    SystemAlertConfig, HISTORY_SOURCE_REPORT, HISTORY_STATUS_FAILED, HISTORY_STATUS_SENT,
};
use serde::Deserialize;

use crate::auth::{RequireAdmin, RequireManager};
use crate::http::AppState;
use crate::notify::{
    send_webhook, AlarmNotification, AlarmSource, EmailTransport, NotifyKind, NotifySealer,
    WebhookConfig,
};
use crate::relay::CommandOutcome;
use crate::render;

use super::command::Cmd;
use super::{non_empty, render as render_page};

/// Fixed history page size (`hlimit` in the template — "table (last 50)").
const HISTORY_LIMIT: i64 = 50;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/settings/notifications",
            get(notifications_page).post(save_settings),
        )
        .route("/settings/notifications/test-email", post(test_email))
        .route("/settings/notifications/test-webhook", post(test_webhook))
}

fn first_hostname(state: &AppState) -> Option<String> {
    state
        .store
        .snapshot()
        .devices
        .values()
        .next()
        .map(|d| d.hostname.clone())
}

/// Every Hub the portal knows, `(hostname, display name)`, for the device
/// alarms card's Hub picker — each Hub has its own recipients and toggles
/// (`device_report_config` is keyed by hostname).
async fn known_hubs(state: &AppState) -> Vec<(String, String)> {
    let snapshot = state.store.snapshot();
    let mut hubs: Vec<(String, String)> = Vec::new();
    for device in snapshot.devices.values() {
        let hostname = device.hostname.clone();
        let meta = super::hub_routes::hub_meta(state, &hostname).await;
        let name = render::hub_display_name(Some(device), meta.as_ref())
            .unwrap_or_else(|| hostname.clone());
        hubs.push((hostname, name));
    }
    hubs.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    hubs
}

/// The Hub a request is about: the asked-for one when the portal knows it,
/// else the first. `None` only while no Hub has reported.
fn pick_hub(hubs: &[(String, String)], asked: Option<&str>) -> Option<String> {
    asked
        .and_then(|a| hubs.iter().find(|(h, _)| h.eq_ignore_ascii_case(a.trim())))
        .or_else(|| hubs.first())
        .map(|(h, _)| h.clone())
}

// ---------------------------------------------------------------------
// Flash slugs (hub_routes' pattern: short codes in the URL). M9: these
// used to map straight to English sentences; now they map to themselves
// (validated against the known set) and `pages/notifications.html` does
// the `("alarms.flash.ok." ~ ok)|t` / `("alarms.flash.err." ~ error)|t`
// lookup, same shape as `auth.errors.*` on pages/login.html. Every slug
// returned here MUST have an `alarms.flash.ok.*`/`alarms.flash.err.*`
// entry in both i18n/{en,cs}/alarms.json — `translation_keys_match_across_
// languages` (src/i18n.rs) only catches an en/cs mismatch, not a slug this
// file emits but the catalogue never defined.
// ---------------------------------------------------------------------

fn ok_slug(code: &str) -> Option<&'static str> {
    match code {
        "saved" => Some("saved"),
        _ => None,
    }
}

/// Unrecognized codes (a hand-built `?err=` query, not one of this file's
/// own `redirect_err()` calls) map to `"unknown"` rather than `None` — the
/// previous message-returning version always showed a generic fallback
/// banner for a non-empty `?err=`, and this preserves that.
fn err_slug(code: &str) -> &'static str {
    const KNOWN: &[&str] = &[
        "bad_provider",
        "from_email_required",
        "bad_from_email",
        "smtp_host_required",
        "bad_port",
        "url_required",
        "bad_url",
        "bad_method",
        "bad_headers",
        "bad_timeout",
        "bad_attempts",
        "bad_recipient",
        "bad_battery",
        "bad_storage",
        "no_device",
        "unknown_hub",
        "bad_lang",
        "public_url_invalid",
        "seal_failed",
        "save_failed",
    ];
    KNOWN
        .iter()
        .find(|&&k| k == code)
        .copied()
        .unwrap_or("unknown")
}

fn hub_qs(hub: Option<&str>) -> String {
    hub.map(|h| format!("&hub={h}")).unwrap_or_default()
}

fn redirect_ok(hub: Option<&str>) -> Response {
    Redirect::to(&format!("/settings/notifications?ok=saved{}", hub_qs(hub))).into_response()
}

fn redirect_err(slug: &str, hub: Option<&str>) -> Response {
    Redirect::to(&format!(
        "/settings/notifications?err={slug}{}",
        hub_qs(hub)
    ))
    .into_response()
}

// ---------------------------------------------------------------------
// GET /settings/notifications
// ---------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct PageQuery {
    hoffset: Option<i64>,
    hub: Option<String>,
    ok: Option<String>,
    err: Option<String>,
}

async fn notifications_page(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Query(q): Query<PageQuery>,
) -> Response {
    let global = match state.db.run(|s| s.get_notify_global_config()).await {
        Ok(global) => global,
        Err(err) => {
            tracing::error!(%err, "failed to load notify global config");
            return (StatusCode::INTERNAL_SERVER_ERROR, "failed to load settings").into_response();
        }
    };
    let system = match state.db.run(|s| s.get_system_alert_config()).await {
        Ok(system) => system,
        Err(err) => {
            tracing::error!(%err, "failed to load system alert config");
            return (StatusCode::INTERNAL_SERVER_ERROR, "failed to load settings").into_response();
        }
    };

    // Per-device config for the picked Hub; never configured -> rest.md
    // defaults (recipients [], disabled, alarm on / clear off / ack off).
    let hubs = known_hubs(&state).await;
    let hostname = pick_hub(&hubs, q.hub.as_deref());
    // The picker marks each Hub whose alarm e-mails reach nobody (UX-059).
    let unreached: Vec<String> = {
        let hosts: Vec<String> = hubs.iter().map(|(h, _)| h.clone()).collect();
        state
            .db
            .run(move |s| {
                let defaults = s.default_alarm_recipients()?;
                Ok(hosts
                    .into_iter()
                    .filter(|h| {
                        let cfg = s.get_device_report_config(h).ok().flatten();
                        !crate::alarms::notifier::has_alarm_recipients(cfg.as_ref(), &defaults)
                    })
                    .collect())
            })
            .await
            .unwrap_or_default()
    };
    let device = match &hostname {
        Some(h) => match ({
            let a0 = h.clone();
            state.db.run(move |s| s.get_device_report_config(&a0))
        })
        .await
        {
            Ok(device) => device,
            Err(err) => {
                tracing::error!(%err, hostname = %h, "failed to load device report config");
                None
            }
        },
        None => None,
    };
    let default_recipients = state
        .db
        .run(|s| s.default_alarm_recipients())
        .await
        .unwrap_or_else(|err| {
            tracing::error!(%err, "failed to load default alarm recipients");
            Vec::new()
        });
    // A Hub without its own row e-mails the default recipients (#31).
    let device_own = device.is_some();
    let device = device.unwrap_or_else(|| DeviceReportConfig {
        hostname: hostname.clone().unwrap_or_default(),
        recipients: Vec::new(),
        enabled: false,
        notify_on_alarm: true,
        notify_on_clear: false,
        notify_on_ack: false,
        updated_at: None,
    });

    let hoffset = q.hoffset.unwrap_or(0).max(0);
    let filter = ReportHistoryFilter {
        limit: HISTORY_LIMIT,
        offset: hoffset,
        ..Default::default()
    };
    let (history, history_total) = match state.db.run(move |s| s.list_report_history(&filter)).await
    {
        Ok(page) => page,
        Err(err) => {
            tracing::error!(%err, "failed to load report history");
            (Vec::new(), 0)
        }
    };
    let history_rows: Vec<_> = history
        .iter()
        .map(|h| {
            context! {
                sent_at => h.sent_at,
                hostname => h.hostname,
                event_type => h.event_type,
                source => h.source,
                recipients => h.recipients,
                subject => h.subject,
                status => h.status,
                error_message => h.error_message,
                provider => h.provider,
            }
        })
        .collect();

    let error = q.err.and_then(non_empty).map(|c| err_slug(&c).to_string());
    let ok =
        q.ok.and_then(non_empty)
            .and_then(|c| ok_slug(&c))
            .map(str::to_string);

    let (notifications_tab, mqtt_tab, maintenance_tab) =
        super::settings_routes::sections(&current_user);
    let ctx = render::page_ctx(
        context! {
            // A section of the settings area, so it carries the tab bar's
            // context; `nav_active` is the area, not this page.
            nav_active => "settings",
            settings_tab => "notifications",
            show_notifications => notifications_tab,
            show_mqtt => mqtt_tab,
            show_maintenance => maintenance_tab,
            // The admin-only half of the page (channels, system alerts):
            // a manager sees the device-alarm card and the history only.
            is_admin => current_user.is_admin(),
            email => context! {
                provider => global.provider,
                from_email => global.from_email,
                from_name => crate::notify::from_name(&global.from_name),
                api_key_set => global.sendgrid_api_key_enc.is_some(),
                smtp => context! {
                    host => global.smtp_host,
                    port => global.smtp_port,
                    username => global.smtp_username,
                    password_set => global.smtp_password_enc.is_some(),
                    use_tls => global.smtp_use_tls,
                    use_ssl => global.smtp_use_ssl,
                },
                public_url => global.public_url,
            },
            webhook => context! {
                enabled => global.webhook_enabled,
                url => global.webhook_url,
                method => global.webhook_method,
                headers_set => global.webhook_headers_enc.is_some(),
                timeout_s => global.webhook_timeout_s,
                max_attempts => global.webhook_max_attempts,
                lifecycle => global.webhook_lifecycle,
            },
            email_lang => global.email_lang,
            hubs => hubs
                .iter()
                .map(|(h, n)| context! { hostname => h, name => n, no_recipients => unreached.contains(h) })
                .collect::<Vec<_>>(),
            default_recipients => default_recipients,
            device => context! {
                own => device_own,
                hostname => hostname,
                recipients => device.recipients,
                enabled => device.enabled,
                notify_on_alarm => device.notify_on_alarm,
                notify_on_clear => device.notify_on_clear,
                notify_on_ack => device.notify_on_ack,
            },
            system => context! {
                recipients => system.recipients,
                battery_enabled => system.battery_enabled,
                battery_threshold => system.battery_threshold,
                network_enabled => system.network_enabled,
                storage_enabled => system.storage_enabled,
                storage_threshold => system.storage_threshold,
                device_offline_enabled => system.device_offline_enabled,
                version_mismatch_enabled => system.version_mismatch_enabled,
                expected_version => system.expected_version,
            },
            history => history_rows,
            history_total => history_total,
            hoffset => hoffset,
            ok => ok,
            error => error,
        },
        Some(&current_user),
    );
    render_page(&state, "pages/notifications.html", ctx)
}

// ---------------------------------------------------------------------
// Email section of the form
// ---------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct EmailForm {
    pub provider: String,
    pub from_email: Option<String>,
    pub from_name: Option<String>,
    pub api_key: Option<String>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<String>,
    pub smtp_username: Option<String>,
    pub smtp_password: Option<String>,
    pub smtp_use_tls: Option<String>,
    pub smtp_use_ssl: Option<String>,
    /// `cs` | `en` — the language e-mails are written in (V18).
    pub email_lang: Option<String>,
    /// The address e-mail links point at (V21, UX-027); blank clears it.
    pub public_url: Option<String>,
    /// HN-16 (Hub test 2026-09-26): a saved secret keeps-on-empty, so without these nothing
    /// could remove one; a password left behind with its username gone also kept SMTP
    /// refusing to send over plaintext.
    pub clear_api_key: Option<String>,
    pub clear_smtp_password: Option<String>,
}

/// Form -> merged config row. Pure except for sealing; validation per
/// rest.md's `PUT /api/reports/config` messages (provider enum, from_email
/// when a provider is set, SMTP host/port ranges). Fields belonging to the
/// *unselected* provider are ignored (the template posts hidden sections
/// too): their stored values are kept as-is. Secrets keep-on-empty: an
/// empty `api_key`/`smtp_password` keeps `current`'s sealed blob.
/// Webhook fields are not on this form and are always carried over.
pub(crate) fn merge_email_form(
    current: &NotifyGlobalConfig,
    form: &EmailForm,
    sealer: &NotifySealer,
) -> Result<NotifyGlobalConfig, &'static str> {
    let provider = form.provider.trim();
    if !matches!(provider, "none" | "sendgrid" | "smtp") {
        return Err("bad_provider");
    }

    let from_email = form.from_email.clone().and_then(non_empty);
    if provider != "none" {
        match &from_email {
            None => return Err("from_email_required"),
            Some(addr) if !is_plausible_email(addr) => return Err("bad_from_email"),
            Some(_) => {}
        }
    } else if let Some(addr) = &from_email {
        if !is_plausible_email(addr) {
            return Err("bad_from_email");
        }
    }

    let mut cfg = current.clone();
    cfg.provider = provider.to_string();
    cfg.from_email = from_email;
    cfg.from_name = form
        .from_name
        .clone()
        .and_then(non_empty)
        .unwrap_or_else(|| proximos_brand::MONITOR.to_string());

    // Selecting SendGrid with no key ever saved is still storable (the
    // test button reports the missing key) — the key may arrive in a later
    // submit.
    if form.clear_api_key.is_some() {
        cfg.sendgrid_api_key_enc = None;
    }
    if form.clear_smtp_password.is_some() {
        cfg.smtp_password_enc = None;
    }
    if provider == "sendgrid" {
        if let Some(key) = form
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        {
            cfg.sendgrid_api_key_enc =
                Some(sealer.seal(key.as_bytes()).map_err(|_| "seal_failed")?);
        }
    }

    if provider == "smtp" {
        let host = form.smtp_host.clone().and_then(non_empty);
        let Some(host) = host else {
            return Err("smtp_host_required");
        };
        let port = match form
            .smtp_port
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            Some(raw) => match raw.parse::<i64>() {
                Ok(p) if (1..=65535).contains(&p) => p,
                _ => return Err("bad_port"),
            },
            None => return Err("bad_port"),
        };
        // LE-9 (local E2E 2026-09-26): the sandbox lets the notifier connect to a few ports
        // only; any other saved fine and then failed every send with "Permission denied".
        if !port_allowed(port) {
            return Err("port_not_allowed");
        }
        cfg.smtp_host = Some(host);
        cfg.smtp_port = Some(port);
        cfg.smtp_username = form.smtp_username.clone().and_then(non_empty);
        cfg.smtp_use_tls = form.smtp_use_tls.is_some();
        cfg.smtp_use_ssl = form.smtp_use_ssl.is_some();
        if let Some(pass) = form.smtp_password.as_deref().filter(|p| !p.is_empty()) {
            cfg.smtp_password_enc = Some(sealer.seal(pass.as_bytes()).map_err(|_| "seal_failed")?);
        }
    }

    // Absent (a form without the field) keeps the stored language.
    if let Some(lang) = form
        .email_lang
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        if !matches!(lang, "cs" | "en") {
            return Err("bad_lang");
        }
        cfg.email_lang = lang.to_string();
    }
    // Absent keeps the stored address, blank clears it (UX-027).
    if let Some(raw) = &form.public_url {
        cfg.public_url = parse_public_url(raw).map_err(|()| "public_url_invalid")?;
    }
    Ok(cfg)
}

/// The portal address an admin typed, as the origin e-mail links start
/// with: `Ok(None)` for blank, else an absolute `http`/`https` URL with a
/// host and nothing past it (no user info, query, fragment, or path beyond
/// `/`), at most 200 characters, stored without the trailing slash
/// (plan/portal/email_links.md §2).
pub(crate) fn parse_public_url(raw: &str) -> Result<Option<String>, ()> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.len() > 200 {
        return Err(());
    }
    let url = reqwest::Url::parse(raw).map_err(|_| ())?;
    let clean = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|h| !h.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path() == "/";
    if !clean {
        return Err(());
    }
    Ok(Some(url.origin().ascii_serialization()))
}

/// A port the notifier's sandbox may connect to (`sandbox::NOTIFIER_CONNECT_PORTS`).
fn port_allowed(port: i64) -> bool {
    u16::try_from(port).is_ok_and(|p| crate::sandbox::NOTIFIER_CONNECT_PORTS.contains(&p))
}

/// Just enough of an email shape check for operator input: one `@` with a
/// non-empty local part and a dot-bearing domain, no whitespace. LE-15: no list separators or
/// display-name brackets either, so `a@x.test,b@x.test` is two addresses, not one.
pub(crate) fn is_plausible_email(s: &str) -> bool {
    if s.chars()
        .any(|c| c.is_whitespace() || matches!(c, ',' | ';' | '<' | '>' | '"'))
        || s.matches('@').count() != 1
    {
        return false;
    }
    match s.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        }
        None => false,
    }
}

// ---------------------------------------------------------------------
// Webhook section of the form
// ---------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct WebhookForm {
    pub enabled: Option<String>,
    pub url: Option<String>,
    pub method: Option<String>,
    pub headers: Option<String>,
    pub timeout_s: Option<String>,
    pub max_attempts: Option<String>,
    /// Also send `alarm.cleared` / `alarm.acknowledged` (V18).
    pub lifecycle: Option<String>,
    /// HN-16: removes the saved headers.
    pub clear_headers: Option<String>,
}

/// `"Name: value"` lines -> header pairs. Blank lines are skipped; a line
/// without a colon or with an empty name is an error (the operator meant
/// something and we'd silently drop it otherwise).
pub(crate) fn parse_headers(text: &str) -> Result<Vec<(String, String)>, ()> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(());
        };
        let name = name.trim();
        if name.is_empty() || name.chars().any(char::is_whitespace) {
            return Err(());
        }
        out.push((name.to_string(), value.trim().to_string()));
    }
    Ok(out)
}

/// Form -> merged config row (webhook half; email fields carried over).
/// Headers keep-on-empty like the other secrets; a non-empty textarea is
/// parsed, JSON-encoded as `[(name, value), ...]`, and sealed.
pub(crate) fn merge_webhook_form(
    current: &NotifyGlobalConfig,
    form: &WebhookForm,
    sealer: &NotifySealer,
) -> Result<NotifyGlobalConfig, &'static str> {
    let enabled = form.enabled.is_some();
    let url = form.url.clone().and_then(non_empty);
    if enabled && url.is_none() {
        return Err("url_required");
    }
    if let Some(u) = &url {
        match reqwest::Url::parse(u) {
            Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {
                // LE-9: the same sandbox port list, the scheme's default port included. Only
                // for a webhook that is on (R-6): an old URL kept with the webhook off must
                // not stop the rest of the settings from saving.
                let port = parsed.port_or_known_default().map_or(0, i64::from);
                if enabled && !port_allowed(port) {
                    return Err("url_port_not_allowed");
                }
            }
            _ => return Err("bad_url"),
        }
    }
    let method = form.method.as_deref().map(str::trim).unwrap_or("POST");
    if !matches!(method, "POST" | "PUT") {
        return Err("bad_method");
    }
    let timeout_s = parse_ranged(form.timeout_s.as_deref(), 1, 60).ok_or("bad_timeout")?;
    let max_attempts = parse_ranged(form.max_attempts.as_deref(), 1, 10).ok_or("bad_attempts")?;

    let mut cfg = current.clone();
    cfg.webhook_enabled = enabled;
    cfg.webhook_url = url;
    cfg.webhook_method = method.to_string();
    cfg.webhook_timeout_s = timeout_s;
    cfg.webhook_max_attempts = max_attempts;

    if form.clear_headers.is_some() {
        cfg.webhook_headers_enc = None;
    }
    if let Some(text) = form
        .headers
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let pairs = parse_headers(text).map_err(|()| "bad_headers")?;
        let json = serde_json::to_vec(&pairs).map_err(|_| "bad_headers")?;
        cfg.webhook_headers_enc = Some(sealer.seal(&json).map_err(|_| "seal_failed")?);
    }
    // Checkbox: absent means off. Off by default (V18) because a receiver
    // that does not read `event` would take a clear for a new alarm.
    cfg.webhook_lifecycle = form.lifecycle.is_some();
    Ok(cfg)
}

/// Trimmed decimal in `[min, max]`, else `None` (missing counts as invalid
/// — both number inputs are always submitted with a value).
fn parse_ranged(raw: Option<&str>, min: i64, max: i64) -> Option<i64> {
    let value = raw?.trim().parse::<i64>().ok()?;
    (min..=max).contains(&value).then_some(value)
}

// ---------------------------------------------------------------------
// Recipient + threshold parsing
// ---------------------------------------------------------------------

/// Recipient textarea -> recipient list: one address per line, or several on a line separated
/// by commas or semicolons (LE-15); trimmed, empties dropped, every survivor must look like an
/// email address.
pub(crate) fn parse_recipients(text: &str) -> Result<Vec<String>, ()> {
    let mut out = Vec::new();
    for line in text.split(['\n', ',', ';']) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !is_plausible_email(line) {
            return Err(());
        }
        out.push(line.to_string());
    }
    Ok(out)
}

/// Float in `[min, max]` rounded to the storage integer, else `None`.
fn parse_ranged_f64(raw: Option<&str>, min: f64, max: f64) -> Option<i64> {
    let value = raw?.trim().parse::<f64>().ok()?;
    (value >= min && value <= max).then_some(value.round() as i64)
}

// ---------------------------------------------------------------------
// POST /settings/notifications (manager/admin)
// ---------------------------------------------------------------------

/// Every section of the page in one submit. The page is one form with one
/// save button, so field names that used to collide across the four forms
/// are prefixed here: `webhook_enabled`, `device_enabled`,
/// `device_recipients`, `system_recipients`. The rest keep the names the
/// merge helpers (and their tests) already speak.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SettingsForm {
    // Email (admin)
    provider: String,
    from_email: Option<String>,
    from_name: Option<String>,
    api_key: Option<String>,
    smtp_host: Option<String>,
    smtp_port: Option<String>,
    smtp_username: Option<String>,
    smtp_password: Option<String>,
    smtp_use_tls: Option<String>,
    smtp_use_ssl: Option<String>,
    clear_api_key: Option<String>,
    clear_smtp_password: Option<String>,
    // Webhook (admin)
    webhook_enabled: Option<String>,
    url: Option<String>,
    method: Option<String>,
    headers: Option<String>,
    timeout_s: Option<String>,
    max_attempts: Option<String>,
    webhook_lifecycle: Option<String>,
    email_lang: Option<String>,
    public_url: Option<String>,
    // Default alarm recipients (manager), decisions.md #31.
    default_recipients: Option<String>,
    clear_headers: Option<String>,
    // Device alarms (manager) — for the Hub named by `hub`.
    hub: Option<String>,
    device_enabled: Option<String>,
    device_recipients: Option<String>,
    notify_on_alarm: Option<String>,
    notify_on_clear: Option<String>,
    notify_on_ack: Option<String>,
    // System alerts (admin)
    system_recipients: Option<String>,
    battery_enabled: Option<String>,
    battery_threshold: Option<String>,
    network_enabled: Option<String>,
    storage_enabled: Option<String>,
    storage_threshold: Option<String>,
    device_offline_enabled: Option<String>,
    version_mismatch_enabled: Option<String>,
    expected_version: Option<String>,
}

impl SettingsForm {
    fn email(&self) -> EmailForm {
        EmailForm {
            provider: self.provider.clone(),
            from_email: self.from_email.clone(),
            from_name: self.from_name.clone(),
            api_key: self.api_key.clone(),
            smtp_host: self.smtp_host.clone(),
            smtp_port: self.smtp_port.clone(),
            smtp_username: self.smtp_username.clone(),
            smtp_password: self.smtp_password.clone(),
            smtp_use_tls: self.smtp_use_tls.clone(),
            smtp_use_ssl: self.smtp_use_ssl.clone(),
            email_lang: self.email_lang.clone(),
            public_url: self.public_url.clone(),
            clear_api_key: self.clear_api_key.clone(),
            clear_smtp_password: self.clear_smtp_password.clone(),
        }
    }

    fn webhook(&self) -> WebhookForm {
        WebhookForm {
            enabled: self.webhook_enabled.clone(),
            url: self.url.clone(),
            method: self.method.clone(),
            headers: self.headers.clone(),
            timeout_s: self.timeout_s.clone(),
            max_attempts: self.max_attempts.clone(),
            lifecycle: self.webhook_lifecycle.clone(),
            clear_headers: self.clear_headers.clone(),
        }
    }

    /// `true` when the operator actually put something in the device
    /// section — the difference between "no hub has reported yet, nothing
    /// to save here" and "these recipients would be dropped on the floor".
    fn touches_device(&self) -> bool {
        self.device_enabled.is_some()
            || self
                .device_recipients
                .as_deref()
                .is_some_and(|r| !r.trim().is_empty())
    }
}

/// Saves the whole page. Validation runs before any write, so a bad
/// threshold leaves the email settings alone instead of half-applying the
/// submit. A manager submits the same form with the admin half absent from
/// their page; those sections are skipped rather than rejected.
async fn save_settings(
    RequireManager(current_user): RequireManager,
    State(state): State<AppState>,
    Form(form): Form<SettingsForm>,
) -> Response {
    let is_admin = current_user.is_admin();

    let hubs = known_hubs(&state).await;
    // A Hub the form names has to be one the portal knows. `pick_hub`'s
    // fallback to the first Hub is for drawing the page; on a save it would
    // write this section into another Hub's row, or delete that row.
    let asked = form.hub.as_deref().map(str::trim).filter(|h| !h.is_empty());
    if asked.is_some_and(|a| !hubs.iter().any(|(h, _)| h.eq_ignore_ascii_case(a))) {
        return redirect_err("unknown_hub", None);
    }
    let hostname = pick_hub(&hubs, asked);
    let back = hostname.clone();
    let back = back.as_deref();
    let device_recipients = match parse_recipients(form.device_recipients.as_deref().unwrap_or(""))
    {
        Ok(recipients) => recipients,
        Err(()) => return redirect_err("bad_recipient", back),
    };
    if hostname.is_none() && form.touches_device() {
        return redirect_err("no_device", back);
    }
    let default_recipients =
        match parse_recipients(form.default_recipients.as_deref().unwrap_or("")) {
            Ok(recipients) => recipients,
            Err(()) => return redirect_err("bad_recipient", back),
        };

    // Admin half: merge both global sections onto one row, validate the
    // system thresholds, and only then write anything.
    let global = if is_admin {
        let current = match state.db.run(|s| s.get_notify_global_config()).await {
            Ok(current) => current,
            Err(err) => {
                tracing::error!(%err, "failed to load notify config for merge");
                return redirect_err("save_failed", back);
            }
        };
        let sealer = &state.sealer;
        let merged = match merge_email_form(&current, &form.email(), sealer) {
            Ok(merged) => merged,
            Err(slug) => return redirect_err(slug, back),
        };
        match merge_webhook_form(&merged, &form.webhook(), sealer) {
            Ok(merged) => Some(merged),
            Err(slug) => return redirect_err(slug, back),
        }
    } else {
        None
    };

    let system = if is_admin {
        let recipients = match parse_recipients(form.system_recipients.as_deref().unwrap_or("")) {
            Ok(recipients) => recipients,
            Err(()) => return redirect_err("bad_recipient", back),
        };
        // The template renders thresholds via `step="0.1"` number inputs
        // but the storage columns are integers — accept a float and round.
        let Some(battery_threshold) =
            parse_ranged_f64(form.battery_threshold.as_deref(), 1.0, 100.0)
        else {
            return redirect_err("bad_battery", back);
        };
        let Some(storage_threshold) =
            parse_ranged_f64(form.storage_threshold.as_deref(), 50.0, 100.0)
        else {
            return redirect_err("bad_storage", back);
        };
        Some(SystemAlertConfig {
            recipients,
            battery_enabled: form.battery_enabled.is_some(),
            battery_threshold,
            network_enabled: form.network_enabled.is_some(),
            storage_enabled: form.storage_enabled.is_some(),
            storage_threshold,
            device_offline_enabled: form.device_offline_enabled.is_some(),
            version_mismatch_enabled: form.version_mismatch_enabled.is_some(),
            expected_version: form.expected_version.clone().and_then(non_empty),
            updated_at: None,
        })
    } else {
        None
    };

    if let Some(global) = global {
        if let Err(err) = state
            .db
            .write(move |s| s.upsert_notify_global_config(&global))
            .await
        {
            tracing::error!(%err, "failed to save notification channels");
            return redirect_err("save_failed", back);
        }
    }
    if let Some(system) = system {
        if let Err(err) = state
            .db
            .write(move |s| s.upsert_system_alert_config(&system))
            .await
        {
            tracing::error!(%err, "failed to save system alert config");
            return redirect_err("save_failed", back);
        }
    }
    let previous_defaults = state
        .db
        .run(|s| s.default_alarm_recipients())
        .await
        .unwrap_or_default();
    let defaults_changed = previous_defaults != default_recipients;
    if defaults_changed {
        let recipients = default_recipients.clone();
        if let Err(err) = state
            .db
            .write(move |s| s.set_default_alarm_recipients(&recipients))
            .await
        {
            tracing::error!(%err, "failed to save default alarm recipients");
            return redirect_err("save_failed", back);
        }
    }
    // A Hub section left empty is no configuration of the Hub's own: its row
    // goes (or is never written), and the Hub follows the defaults (#31).
    if let Some(hostname) = hostname.clone().filter(|_| !form.touches_device()) {
        if let Err(err) = state
            .db
            .write(move |s| s.delete_device_report_config(&hostname))
            .await
        {
            tracing::error!(%err, "failed to drop device report config");
            return redirect_err("save_failed", back);
        }
    } else if let Some(hostname) = hostname {
        let cfg = DeviceReportConfig {
            hostname: hostname.clone(),
            recipients: device_recipients,
            enabled: form.device_enabled.is_some(),
            notify_on_alarm: form.notify_on_alarm.is_some(),
            notify_on_clear: form.notify_on_clear.is_some(),
            notify_on_ack: form.notify_on_ack.is_some(),
            updated_at: None,
        };
        if let Err(err) = state
            .db
            .write(move |s| s.upsert_device_report_config(&cfg))
            .await
        {
            tracing::error!(%err, hostname, "failed to save device report config");
            return redirect_err("save_failed", back);
        }
    }

    state
        .auth
        .audit(
            "notify_config",
            Some(&current_user),
            None,
            Some("notifications"),
            defaults_changed.then(|| {
                serde_json::json!({ "default_alarm_recipients": default_recipients }).to_string()
            }),
            true,
        )
        .await;
    redirect_ok(back)
}

// ---------------------------------------------------------------------
// POST /settings/notifications/test-email + test-webhook (admin, htmx)
// ---------------------------------------------------------------------

/// A `CommandOutcome`-shaped result for `partials/command_status.html` —
/// no request_id/handshake, per the template header's blessed stretch.
/// A test button's result sentence in the request's language (C36-6, local smoke test:
/// every result and refusal was English on cs pages).
fn tr(key: &str) -> String {
    crate::i18n::lookup(
        crate::i18n::current_lang(),
        &format!("alarms.notify.test_result.{key}"),
    )
}

fn test_outcome(command_type: &str, success: bool, message: String) -> CommandOutcome {
    CommandOutcome {
        request_id: None,
        command_type: command_type.to_string(),
        status: if success { "success" } else { "error" },
        message: Some(message),
        error_code: None,
        result: None,
    }
}

/// The synthetic Test-kind notification both test buttons render/send.
fn test_notification(hostname: &str, lang: &str) -> AlarmNotification {
    AlarmNotification {
        kind: NotifyKind::Test,
        severity: "info".to_string(),
        alarm_type: "test".to_string(),
        source: AlarmSource::System,
        value: None,
        threshold: None,
        message: format!("Test notification from {}", proximos_brand::MONITOR),
        hostname: hostname.to_string(),
        device_label: None,
        dev_eui: None,
        serial_number: None,
        mac: None,
        sensor_name: "test".to_string(),
        sensor_id: "test".to_string(),
        line: None,
        location: None,
        triggered_at: time::OffsetDateTime::now_utc(),
        alarm_id: "test".to_string(),
        lang: lang.to_string(),
        quantity: None,
        unit: None,
        device_name: None,
    }
}

/// Builds the [`EmailTransport`] for the stored provider config, or a
/// human-readable reason it can't be built.
fn build_transport(
    cfg: &NotifyGlobalConfig,
    sealer: &NotifySealer,
) -> Result<EmailTransport, String> {
    match cfg.provider.as_str() {
        "sendgrid" => {
            let Some(blob) = &cfg.sendgrid_api_key_enc else {
                return Err(tr("no_sendgrid_key"));
            };
            let api_key = sealer
                .unseal_string(blob)
                .map_err(|_| tr("sendgrid_key_undecryptable"))?;
            Ok(EmailTransport::Sendgrid {
                api_key: api_key.to_string(),
            })
        }
        "smtp" => {
            let Some(host) = cfg.smtp_host.clone() else {
                return Err(tr("no_smtp_host"));
            };
            let Some(port) = cfg.smtp_port.filter(|p| (1..=65535).contains(p)) else {
                return Err(tr("no_smtp_port"));
            };
            let password = match &cfg.smtp_password_enc {
                Some(blob) => Some(
                    sealer
                        .unseal_string(blob)
                        .map_err(|_| tr("smtp_password_undecryptable"))?
                        .to_string(),
                ),
                None => None,
            };
            Ok(EmailTransport::Smtp {
                host,
                port: port as u16,
                username: cfg.smtp_username.clone(),
                password,
                use_tls: cfg.smtp_use_tls,
                use_ssl: cfg.smtp_use_ssl,
            })
        }
        _ => Err(tr("no_provider")),
    }
}

/// Best-effort history row for a test delivery (`event_type: "test"`) —
/// a bookkeeping failure is logged, never surfaced over the test result.
async fn record_test_history(
    state: &AppState,
    hostname: Option<String>,
    recipients: Vec<String>,
    subject: Option<String>,
    success: bool,
    error_message: Option<String>,
    provider: Option<String>,
) {
    let row = NewReportHistory {
        hostname,
        event_type: "test".to_string(),
        recipients,
        subject,
        message: None,
        status: if success {
            HISTORY_STATUS_SENT.to_string()
        } else {
            HISTORY_STATUS_FAILED.to_string()
        },
        error_message,
        provider,
        source: HISTORY_SOURCE_REPORT.to_string(),
        sent_at: None,
    };
    // The delivery record spills rather than being dropped (`crate::durable`),
    // so it carries its own busy handling: `run`, not `write`.
    let spill = std::sync::Arc::clone(&state.spill);
    if let Err(err) = state
        .db
        .run(move |s| crate::durable::insert_report_history(&spill, s, &row).map(|_| ()))
        .await
    {
        tracing::warn!(%err, "failed to record test delivery in report history");
    }
}

async fn test_email(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
) -> Response {
    const CMD: &str = "test_email";
    let cmd = Cmd::new(&state, &current_user, CMD);

    let cfg = match state.db.run(|s| s.get_notify_global_config()).await {
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::error!(%err, "failed to load notify config for test email");
            return cmd.done(test_outcome(CMD, false, tr("load_failed")));
        }
    };

    let transport = match build_transport(&cfg, &state.sealer) {
        Ok(transport) => transport,
        Err(reason) => return cmd.done(test_outcome(CMD, false, reason)),
    };
    let Some(from_email) = cfg.from_email.clone() else {
        return cmd.done(test_outcome(CMD, false, tr("no_from")));
    };

    // Recipients: both places a real notification goes, the per-device list and the
    // system-alert list (HN-17, Hub test 2026-09-26: the system list went untested whenever the
    // device list was set), each address once.
    let hostname = first_hostname(&state);
    let mut recipients: Vec<String> = Vec::new();
    if let Some(h) = &hostname {
        if let Ok(Some(device)) = ({
            let a0 = h.clone();
            state.db.run(move |s| s.get_device_report_config(&a0))
        })
        .await
        {
            recipients.extend(device.recipients);
        }
    }
    if let Ok(system) = state.db.run(|s| s.get_system_alert_config()).await {
        for r in system.recipients {
            if !recipients.iter().any(|x| x.eq_ignore_ascii_case(&r)) {
                recipients.push(r);
            }
        }
    }
    if recipients.is_empty() {
        return cmd.done(test_outcome(CMD, false, tr("no_recipients")));
    }

    let notification = test_notification(hostname.as_deref().unwrap_or("portal"), &cfg.email_lang);
    let subject = notification.email_subject();
    let body = notification.email_body(cfg.public_url.as_deref());
    let outcome = transport
        .send(
            &from_email,
            crate::notify::from_name(&cfg.from_name),
            &recipients,
            &subject,
            &body.to_text(),
            &body.to_html(),
        )
        .await;

    // C36-6: the sentence in the UI language, the transport's own detail after it.
    let message = if outcome.success {
        tr("email_sent").replace("{n}", &recipients.len().to_string())
    } else {
        tr("email_failed").replace("{detail}", &outcome.message)
    };
    record_test_history(
        &state,
        hostname,
        recipients,
        Some(subject),
        outcome.success,
        (!outcome.success).then(|| outcome.message.clone()),
        Some(cfg.provider.clone()),
    )
    .await;

    cmd.done(test_outcome(CMD, outcome.success, message))
}

async fn test_webhook(
    RequireAdmin(current_user): RequireAdmin,
    State(state): State<AppState>,
) -> Response {
    const CMD: &str = "test_webhook";
    let cmd = Cmd::new(&state, &current_user, CMD);

    let cfg = match state.db.run(|s| s.get_notify_global_config()).await {
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::error!(%err, "failed to load notify config for test webhook");
            return cmd.done(test_outcome(CMD, false, tr("load_failed")));
        }
    };
    let Some(url) = cfg.webhook_url.clone() else {
        return cmd.done(test_outcome(CMD, false, tr("no_webhook_url")));
    };

    let headers: Vec<(String, String)> = match &cfg.webhook_headers_enc {
        Some(blob) => {
            let sealer = &state.sealer;
            match sealer
                .unseal(blob)
                .ok()
                .and_then(|json| serde_json::from_slice(&json).ok())
            {
                Some(pairs) => pairs,
                None => return cmd.done(test_outcome(CMD, false, tr("headers_undecryptable"))),
            }
        }
        None => Vec::new(),
    };

    let hostname = first_hostname(&state);
    let notification = test_notification(hostname.as_deref().unwrap_or("portal"), &cfg.email_lang);
    let Some(payload) = notification.webhook_payload(false) else {
        // Test kind always has a webhook event; defensive rather than unwrap.
        return cmd.done(test_outcome(CMD, false, tr("payload_failed")));
    };

    let webhook_cfg = WebhookConfig {
        url: url.clone(),
        method: cfg.webhook_method.clone(),
        headers,
        timeout_s: cfg.webhook_timeout_s.clamp(1, 60) as u64,
        // v1 parity: the manual test is a single attempt, no retry ladder.
        max_attempts: 1,
    };
    let outcome = send_webhook(&webhook_cfg, &payload).await;
    // `send_webhook` already names the status of a delivered or refused
    // request ("delivered (HTTP 200)", "HTTP 500"); appending it again read
    // "delivered (HTTP 200) (HTTP 200)". Only a transport error that still
    // carries a status needs it added (HN-10). C36-6: after a sentence in the
    // UI language.
    let detail = match outcome.status {
        Some(status) if !outcome.message.contains("HTTP ") => {
            format!("{} (HTTP {status})", outcome.message)
        }
        _ => outcome.message.clone(),
    };
    let message = tr(if outcome.success {
        "webhook_ok"
    } else {
        "webhook_failed"
    })
    .replace("{detail}", &detail);

    record_test_history(
        &state,
        hostname,
        vec![crate::notify::webhook::redact_url(&url)],
        None,
        outcome.success,
        (!outcome.success).then(|| outcome.message.clone()),
        None,
    )
    .await;

    cmd.done(test_outcome(CMD, outcome.success, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HN-16: a saved secret can be removed.
    #[test]
    fn saved_secrets_can_be_cleared() {
        let sealer = NotifySealer::new(None);
        let mut current = base_config();
        current.smtp_password_enc = Some(sealer.seal(b"pw").expect("seal"));
        current.sendgrid_api_key_enc = Some(sealer.seal(b"key").expect("seal"));
        current.webhook_headers_enc = Some(sealer.seal(b"[]").expect("seal"));
        let email = EmailForm {
            provider: "none".into(),
            clear_api_key: Some("true".into()),
            clear_smtp_password: Some("true".into()),
            ..Default::default()
        };
        let merged = merge_email_form(&current, &email, &sealer).expect("merge");
        assert!(merged.smtp_password_enc.is_none() && merged.sendgrid_api_key_enc.is_none());
        let webhook = WebhookForm {
            method: Some("POST".into()),
            timeout_s: Some("10".into()),
            max_attempts: Some("3".into()),
            clear_headers: Some("true".into()),
            ..Default::default()
        };
        let merged = merge_webhook_form(&current, &webhook, &sealer).expect("merge");
        assert!(merged.webhook_headers_enc.is_none());
    }

    /// LE-15: a comma or semicolon separates two addresses; a list pasted into one line is not
    /// one address.
    #[test]
    fn recipients_split_on_commas_and_semicolons() {
        assert_eq!(
            parse_recipients("a@x.test, b@x.test;c@x.test\nd@x.test"),
            Ok(vec![
                "a@x.test".to_string(),
                "b@x.test".to_string(),
                "c@x.test".to_string(),
                "d@x.test".to_string()
            ])
        );
        assert!(!is_plausible_email("a@x.test,b@x.test"));
        assert!(!is_plausible_email("Jan <jan@x.test>"));
    }

    /// LE-9: only the ports the notifier's sandbox may connect to.
    #[test]
    fn only_sandbox_ports_are_accepted() {
        assert!(port_allowed(587) && port_allowed(2525) && port_allowed(443));
        assert!(!port_allowed(1025) && !port_allowed(9099) && !port_allowed(0));
    }

    fn base_config() -> NotifyGlobalConfig {
        NotifyGlobalConfig {
            provider: "none".to_string(),
            from_email: None,
            from_name: "ProXimos Monitor".to_string(),
            sendgrid_api_key_enc: None,
            smtp_host: None,
            smtp_port: None,
            smtp_username: None,
            smtp_password_enc: None,
            smtp_use_tls: true,
            smtp_use_ssl: false,
            webhook_enabled: false,
            webhook_url: None,
            webhook_method: "POST".to_string(),
            webhook_headers_enc: None,
            webhook_timeout_s: 10,
            webhook_max_attempts: 3,
            updated_at: None,
            email_lang: "cs".into(),
            webhook_lifecycle: false,
            public_url: None,
        }
    }

    fn sendgrid_form(api_key: &str) -> EmailForm {
        EmailForm {
            provider: "sendgrid".to_string(),
            from_email: Some("alerts@example.com".to_string()),
            from_name: Some("ProXimos Monitor".to_string()),
            api_key: Some(api_key.to_string()),
            ..EmailForm::default()
        }
    }

    // --- secrets keep-on-empty ---

    #[test]
    fn empty_api_key_keeps_existing_sealed_blob() {
        let sealer = NotifySealer::new(None);
        let mut current = base_config();
        current.provider = "sendgrid".to_string();
        let blob = sealer.seal(b"SG.original").expect("seal");
        current.sendgrid_api_key_enc = Some(blob.clone());

        let merged = merge_email_form(&current, &sendgrid_form(""), &sealer).expect("merge");
        assert_eq!(
            merged.sendgrid_api_key_enc,
            Some(blob),
            "empty input must keep the stored blob byte-for-byte"
        );
    }

    #[test]
    fn new_api_key_replaces_blob_and_round_trips() {
        let sealer = NotifySealer::new(None);
        let mut current = base_config();
        current.sendgrid_api_key_enc = Some(sealer.seal(b"SG.old").expect("seal"));

        let merged = merge_email_form(&current, &sendgrid_form("SG.new"), &sealer).expect("merge");
        let blob = merged.sendgrid_api_key_enc.expect("blob present");
        assert_eq!(&*sealer.unseal_string(&blob).expect("unseal"), "SG.new");
    }

    #[test]
    fn empty_smtp_password_keeps_existing_blob() {
        let sealer = NotifySealer::new(None);
        let mut current = base_config();
        let blob = sealer.seal(b"old-password").expect("seal");
        current.smtp_password_enc = Some(blob.clone());

        let form = EmailForm {
            provider: "smtp".to_string(),
            from_email: Some("alerts@example.com".to_string()),
            smtp_host: Some("mail.example.com".to_string()),
            smtp_port: Some("587".to_string()),
            smtp_password: Some(String::new()),
            smtp_use_tls: Some("true".to_string()),
            ..EmailForm::default()
        };
        let merged = merge_email_form(&current, &form, &sealer).expect("merge");
        assert_eq!(merged.smtp_password_enc, Some(blob));
        assert_eq!(merged.smtp_host.as_deref(), Some("mail.example.com"));
        assert_eq!(merged.smtp_port, Some(587));
    }

    #[test]
    fn unselected_provider_fields_are_ignored_not_cleared() {
        let sealer = NotifySealer::new(None);
        let mut current = base_config();
        current.smtp_host = Some("mail.example.com".to_string());
        current.smtp_port = Some(465);
        let password = sealer.seal(b"pw").expect("seal");
        current.smtp_password_enc = Some(password.clone());

        // Switching to SendGrid: SMTP fields keep their stored values even
        // though the (hidden) SMTP section posted empties.
        let merged = merge_email_form(&current, &sendgrid_form("SG.k"), &sealer).expect("merge");
        assert_eq!(merged.smtp_host.as_deref(), Some("mail.example.com"));
        assert_eq!(merged.smtp_port, Some(465));
        assert_eq!(merged.smtp_password_enc, Some(password));
    }

    // --- email form validation ---

    #[test]
    fn email_form_validation_slugs() {
        let sealer = NotifySealer::new(None);
        let current = base_config();

        let mut form = sendgrid_form("k");
        form.provider = "carrier-pigeon".to_string();
        assert_eq!(
            merge_email_form(&current, &form, &sealer).unwrap_err(),
            "bad_provider"
        );

        let mut form = sendgrid_form("k");
        form.from_email = Some("  ".to_string());
        assert_eq!(
            merge_email_form(&current, &form, &sealer).unwrap_err(),
            "from_email_required"
        );

        let mut form = sendgrid_form("k");
        form.from_email = Some("not-an-address".to_string());
        assert_eq!(
            merge_email_form(&current, &form, &sealer).unwrap_err(),
            "bad_from_email"
        );

        let form = EmailForm {
            provider: "smtp".to_string(),
            from_email: Some("a@b.cz".to_string()),
            smtp_port: Some("587".to_string()),
            ..EmailForm::default()
        };
        assert_eq!(
            merge_email_form(&current, &form, &sealer).unwrap_err(),
            "smtp_host_required"
        );

        for bad_port in ["", "0", "65536", "nope"] {
            let form = EmailForm {
                provider: "smtp".to_string(),
                from_email: Some("a@b.cz".to_string()),
                smtp_host: Some("mail.example.com".to_string()),
                smtp_port: Some(bad_port.to_string()),
                ..EmailForm::default()
            };
            assert_eq!(
                merge_email_form(&current, &form, &sealer).unwrap_err(),
                "bad_port",
                "port {bad_port:?}"
            );
        }

        // provider none with no from_email is a legal "turn it off" submit.
        let form = EmailForm {
            provider: "none".to_string(),
            ..EmailForm::default()
        };
        assert!(merge_email_form(&current, &form, &sealer).is_ok());
    }

    // --- headers parser ---

    #[test]
    fn headers_parser_valid_lines() {
        let parsed =
            parse_headers("X-Token: secret\n\n  Content-Type:  text/json  \n").expect("parse");
        assert_eq!(
            parsed,
            vec![
                ("X-Token".to_string(), "secret".to_string()),
                ("Content-Type".to_string(), "text/json".to_string()),
            ]
        );
    }

    #[test]
    fn headers_parser_rejects_malformed_lines() {
        assert!(parse_headers("just-a-token-no-colon").is_err());
        assert!(parse_headers(": value-without-name").is_err());
        assert!(parse_headers("Bad Name: value").is_err());
        assert_eq!(parse_headers("").expect("empty ok"), Vec::new());
    }

    #[test]
    fn headers_value_may_contain_colons() {
        let parsed = parse_headers("Authorization: Bearer a:b:c").expect("parse");
        assert_eq!(parsed[0].1, "Bearer a:b:c");
    }

    // --- webhook form ---

    fn webhook_form(url: &str) -> WebhookForm {
        WebhookForm {
            enabled: Some("true".to_string()),
            url: Some(url.to_string()),
            method: Some("POST".to_string()),
            headers: None,
            timeout_s: Some("10".to_string()),
            max_attempts: Some("3".to_string()),
            lifecycle: None,
            clear_headers: None,
        }
    }

    #[test]
    fn webhook_form_validation_slugs() {
        let sealer = NotifySealer::new(None);
        let current = base_config();

        let mut form = webhook_form("https://example.com/hook");
        form.url = None;
        assert_eq!(
            merge_webhook_form(&current, &form, &sealer).unwrap_err(),
            "url_required"
        );

        assert_eq!(
            merge_webhook_form(&current, &webhook_form("ftp://example.com"), &sealer).unwrap_err(),
            "bad_url"
        );

        let mut form = webhook_form("https://example.com/hook");
        form.method = Some("DELETE".to_string());
        assert_eq!(
            merge_webhook_form(&current, &form, &sealer).unwrap_err(),
            "bad_method"
        );

        let mut form = webhook_form("https://example.com/hook");
        form.timeout_s = Some("61".to_string());
        assert_eq!(
            merge_webhook_form(&current, &form, &sealer).unwrap_err(),
            "bad_timeout"
        );

        let mut form = webhook_form("https://example.com/hook");
        form.max_attempts = Some("0".to_string());
        assert_eq!(
            merge_webhook_form(&current, &form, &sealer).unwrap_err(),
            "bad_attempts"
        );

        let mut form = webhook_form("https://example.com/hook");
        form.headers = Some("no colon here".to_string());
        assert_eq!(
            merge_webhook_form(&current, &form, &sealer).unwrap_err(),
            "bad_headers"
        );
    }

    #[test]
    fn webhook_headers_keep_on_empty_and_seal_on_new() {
        let sealer = NotifySealer::new(None);
        let mut current = base_config();
        let blob = sealer.seal(b"[[\"X\",\"y\"]]").expect("seal");
        current.webhook_headers_enc = Some(blob.clone());

        // Blank textarea keeps the stored blob.
        let merged = merge_webhook_form(&current, &webhook_form("https://example.com/h"), &sealer)
            .expect("merge");
        assert_eq!(merged.webhook_headers_enc, Some(blob));

        // New lines replace it with a sealed JSON pair list.
        let mut form = webhook_form("https://example.com/h");
        form.headers = Some("X-Token: t".to_string());
        let merged = merge_webhook_form(&current, &form, &sealer).expect("merge");
        let sealed = merged.webhook_headers_enc.expect("sealed");
        let json = sealer.unseal(&sealed).expect("unseal");
        let pairs: Vec<(String, String)> = serde_json::from_slice(&json).expect("json");
        assert_eq!(pairs, vec![("X-Token".to_string(), "t".to_string())]);
    }

    #[test]
    fn webhook_disabled_without_url_is_legal() {
        let sealer = NotifySealer::new(None);
        let form = WebhookForm {
            enabled: None,
            url: None,
            method: Some("POST".to_string()),
            headers: None,
            timeout_s: Some("10".to_string()),
            max_attempts: Some("3".to_string()),
            lifecycle: None,
            clear_headers: None,
        };
        let merged = merge_webhook_form(&base_config(), &form, &sealer).expect("merge");
        assert!(!merged.webhook_enabled);
        assert_eq!(merged.webhook_url, None);
    }

    // --- recipients parser ---

    #[test]
    fn recipients_parser_trims_and_drops_empties() {
        let parsed = parse_recipients("  ops@example.com \n\n night@example.com\n").expect("parse");
        assert_eq!(parsed, vec!["ops@example.com", "night@example.com"]);
        assert_eq!(
            parse_recipients("").expect("empty ok"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn recipients_parser_rejects_non_addresses() {
        assert!(parse_recipients("ops@example.com\nnot an email").is_err());
        assert!(parse_recipients("@example.com").is_err());
        assert!(parse_recipients("ops@nodot").is_err());
    }

    // --- misc pure helpers ---

    #[test]
    fn ranged_parsers() {
        assert_eq!(parse_ranged(Some("30"), 1, 60), Some(30));
        assert_eq!(parse_ranged(Some(" 1 "), 1, 60), Some(1));
        assert_eq!(parse_ranged(Some("0"), 1, 60), None);
        assert_eq!(parse_ranged(None, 1, 60), None);
        assert_eq!(parse_ranged_f64(Some("20.4"), 1.0, 100.0), Some(20));
        assert_eq!(parse_ranged_f64(Some("0.5"), 1.0, 100.0), None);
        assert_eq!(parse_ranged_f64(Some("101"), 1.0, 100.0), None);
    }

    /// plan/portal/email_links.md §2: an origin or nothing.
    #[test]
    fn public_url_validation_table() {
        for (raw, want) in [
            (
                "https://proximos-ce3d59f8.local",
                "https://proximos-ce3d59f8.local",
            ),
            (
                "https://proximos-ce3d59f8.local/",
                "https://proximos-ce3d59f8.local",
            ),
            ("  http://10.0.0.5:8088/ ", "http://10.0.0.5:8088"),
            ("HTTPS://Portal.Example.com", "https://portal.example.com"),
        ] {
            assert_eq!(parse_public_url(raw), Ok(Some(want.to_string())), "{raw}");
        }
        assert_eq!(parse_public_url(""), Ok(None));
        assert_eq!(parse_public_url("   "), Ok(None));
        for raw in [
            "https://portal.example.com/proximos",
            "https://portal.example.com/?a=1",
            "https://portal.example.com/?",
            "https://portal.example.com/#top",
            "https://user:pw@portal.example.com",
            "https://user@portal.example.com",
            "ftp://portal.example.com",
            "javascript:alert(1)",
            "portal.example.com",
            "/alarms",
        ] {
            assert_eq!(parse_public_url(raw), Err(()), "{raw}");
        }
        let long = format!("https://{}.example.com", "a".repeat(200));
        assert_eq!(parse_public_url(&long), Err(()));
        assert_eq!(err_slug("public_url_invalid"), "public_url_invalid");
    }

    #[test]
    fn err_slug_has_a_fallback() {
        assert_eq!(err_slug("nobody-sends-this"), "unknown");
        assert!(ok_slug("nobody-sends-this").is_none());
    }

    mod integration {
        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use tower::ServiceExt;

        use crate::auth::{Role, SESSION_COOKIE};
        use crate::routes::test_support::{login, login_as_role, test_state};

        async fn send(
            router: &axum::Router,
            method: &str,
            path: &str,
            token: Option<&str>,
            form_body: Option<&str>,
        ) -> axum::http::Response<Body> {
            let mut builder = Request::builder().method(method).uri(path);
            if let Some(token) = token {
                builder = builder.header(header::COOKIE, format!("{SESSION_COOKIE}={token}"));
            }
            let request = match form_body {
                Some(body) => builder
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body.to_string()))
                    .expect("request"),
                None => builder.body(Body::empty()).expect("request"),
            };
            router.clone().oneshot(request).await.expect("response")
        }

        async fn body_text(response: axum::http::Response<Body>) -> String {
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            String::from_utf8(bytes.to_vec()).expect("utf-8")
        }

        fn location(response: &axum::http::Response<Body>) -> String {
            response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        }

        #[tokio::test]
        async fn viewer_gets_403_on_page_and_every_post() {
            let (state, _dir) = test_state().await;
            let token = login_as_role(&state, Role::Viewer, "viewer1").await;
            let router = super::super::router().with_state(state);

            let response = send(
                &router,
                "GET",
                "/settings/notifications",
                Some(&token),
                None,
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "page is manager+");

            for path in [
                "/settings/notifications",
                "/settings/notifications/test-email",
                "/settings/notifications/test-webhook",
            ] {
                let response = send(&router, "POST", path, Some(&token), Some("")).await;
                assert_eq!(response.status(), StatusCode::FORBIDDEN, "path {path}");
            }
        }

        /// A manager may save the page, but only the device half of it: the
        /// admin sections their page never renders must not be applied even
        /// when the fields are posted by hand.
        #[tokio::test]
        async fn manager_saves_device_section_only() {
            let (state, _dir) = test_state().await;
            let token = login_as_role(&state, Role::Manager, "manager1").await;
            let router = super::super::router().with_state(state.clone());

            let response = send(
                &router,
                "GET",
                "/settings/notifications",
                Some(&token),
                None,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);

            for path in [
                "/settings/notifications/test-email",
                "/settings/notifications/test-webhook",
            ] {
                let response = send(&router, "POST", path, Some(&token), Some("")).await;
                assert_eq!(response.status(), StatusCode::FORBIDDEN, "path {path}");
            }

            let response = send(
                &router,
                "POST",
                "/settings/notifications",
                Some(&token),
                Some("provider=sendgrid&api_key=SG.manager&battery_threshold=5&public_url=https%3A%2F%2Fevil.example"),
            )
            .await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            assert_eq!(
                location(&response),
                "/settings/notifications?ok=saved",
                "the admin half is skipped, not rejected"
            );
            let global = state
                .db
                .run(|s| s.get_notify_global_config())
                .await
                .expect("load config");
            assert_eq!(global.provider, "none", "manager cannot set the provider");
            assert!(global.sendgrid_api_key_enc.is_none());
            assert_eq!(global.public_url, None, "public_url is admin-only");

            // Recipients with no hub reported yet would be dropped on the
            // floor, so that still flashes no_device.
            let response = send(
                &router,
                "POST",
                "/settings/notifications",
                Some(&token),
                Some("device_recipients=ops%40example.com"),
            )
            .await;
            assert_eq!(location(&response), "/settings/notifications?err=no_device");
        }

        #[tokio::test]
        async fn anonymous_requests_redirect_to_login() {
            let (state, _dir) = test_state().await;
            let router = super::super::router().with_state(state);

            // A GET keeps its target for after the login (UX-027); a POST
            // has nothing to come back to.
            for (method, path, want) in [
                (
                    "GET",
                    "/settings/notifications",
                    "/login?next=%2Fsettings%2Fnotifications",
                ),
                ("POST", "/settings/notifications", "/login"),
            ] {
                let body = (method == "POST").then_some("");
                let response = send(&router, method, path, None, body).await;
                assert_eq!(response.status(), StatusCode::SEE_OTHER, "{method} {path}");
                assert_eq!(location(&response), want, "{method} {path}");
            }
        }

        /// C36-6: a test button answers in the page's language.
        #[tokio::test]
        async fn a_test_button_answers_in_the_ui_language() {
            let (state, _dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router()
                .with_state(state.clone())
                .layer(axum::middleware::from_fn(crate::i18n::lang_middleware));
            let request = Request::builder()
                .method("POST")
                .uri("/settings/notifications/test-email")
                .header(
                    header::COOKIE,
                    format!("{SESSION_COOKIE}={token}; {}=cs", crate::i18n::LANG_COOKIE),
                )
                .body(Body::empty())
                .expect("request");
            let response = router.oneshot(request).await.expect("response");
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            let body = String::from_utf8_lossy(&body);
            assert!(
                body.contains("Není nastaven poskytovatel e-mailu."),
                "{body}"
            );
        }

        #[tokio::test]
        async fn admin_save_seals_secret_and_keeps_it_on_empty_resubmit() {
            let (state, _dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state.clone());

            // First save: a new key gets sealed.
            let response = send(
                &router,
                "POST",
                "/settings/notifications",
                Some(&token),
                Some("provider=sendgrid&from_email=alerts%40example.com&from_name=ProXimos&api_key=SG.first&timeout_s=10&max_attempts=3&battery_threshold=20&storage_threshold=90"),
            )
            .await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            assert_eq!(location(&response), "/settings/notifications?ok=saved");

            let saved = state
                .db
                .run(|s| s.get_notify_global_config())
                .await
                .expect("load config");
            assert_eq!(saved.provider, "sendgrid");
            let blob = saved.sendgrid_api_key_enc.clone().expect("sealed key");
            assert_eq!(
                &*state.sealer.unseal_string(&blob).expect("unseal"),
                "SG.first"
            );

            // Resubmit with the api_key input left blank: blob unchanged.
            let response = send(
                &router,
                "POST",
                "/settings/notifications",
                Some(&token),
                Some("provider=sendgrid&from_email=alerts%40example.com&from_name=ProXimos&api_key=&timeout_s=10&max_attempts=3&battery_threshold=20&storage_threshold=90"),
            )
            .await;
            assert_eq!(location(&response), "/settings/notifications?ok=saved");
            let resaved = state
                .db
                .run(|s| s.get_notify_global_config())
                .await
                .expect("reload config");
            assert_eq!(
                resaved.sendgrid_api_key_enc,
                Some(blob),
                "empty submit must keep the sealed blob byte-for-byte"
            );
        }

        /// UX-027: the admin saves the portal address as an origin; a bad one
        /// flashes `public_url_invalid` and keeps the stored address; blank
        /// clears it. The page shows the field and, while empty, the hint.
        #[tokio::test]
        async fn admin_saves_the_public_url() {
            let (state, _dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state.clone());
            let form = |url: &str| {
                format!("provider=none&timeout_s=10&max_attempts=3&battery_threshold=20&storage_threshold=90&public_url={url}")
            };
            let stored = || async {
                state
                    .db
                    .run(|s| s.get_notify_global_config())
                    .await
                    .expect("load config")
                    .public_url
            };

            let page = send(
                &router,
                "GET",
                "/settings/notifications",
                Some(&token),
                None,
            )
            .await;
            let html = body_text(page).await;
            assert!(html.contains("name=\"public_url\""), "{html}");
            assert!(html.contains("window.location.origin"), "{html}");

            let ok = form("https%3A%2F%2Fproximos-ce3d59f8.local%2F");
            let response = send(
                &router,
                "POST",
                "/settings/notifications",
                Some(&token),
                Some(&ok),
            )
            .await;
            assert_eq!(location(&response), "/settings/notifications?ok=saved");
            assert_eq!(
                stored().await.as_deref(),
                Some("https://proximos-ce3d59f8.local")
            );

            let bad = form("https%3A%2F%2Fp.local%2Fx%3Fy%3D1");
            let response = send(
                &router,
                "POST",
                "/settings/notifications",
                Some(&token),
                Some(&bad),
            )
            .await;
            assert_eq!(
                location(&response),
                "/settings/notifications?err=public_url_invalid"
            );
            assert_eq!(
                stored().await.as_deref(),
                Some("https://proximos-ce3d59f8.local")
            );

            let response = send(
                &router,
                "POST",
                "/settings/notifications",
                Some(&token),
                Some(&form("")),
            )
            .await;
            assert_eq!(location(&response), "/settings/notifications?ok=saved");
            assert_eq!(stored().await, None);
        }

        #[tokio::test]
        async fn admin_validation_failure_flashes_err_slug_and_writes_nothing() {
            let (state, _dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state.clone());

            let response = send(
                &router,
                "POST",
                "/settings/notifications",
                Some(&token),
                Some("provider=sendgrid&from_email=&battery_threshold=20&storage_threshold=90"),
            )
            .await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            assert_eq!(
                location(&response),
                "/settings/notifications?err=from_email_required"
            );

            // A bad threshold rejects the whole submit, email included.
            let response = send(
                &router,
                "POST",
                "/settings/notifications",
                Some(&token),
                Some("provider=sendgrid&from_email=alerts%40example.com&api_key=SG.k&timeout_s=10&max_attempts=3&battery_threshold=0&storage_threshold=90"),
            )
            .await;
            assert_eq!(
                location(&response),
                "/settings/notifications?err=bad_battery"
            );
            let global = state
                .db
                .run(|s| s.get_notify_global_config())
                .await
                .expect("load config");
            assert_eq!(
                global.provider, "none",
                "a rejected submit must not half-apply"
            );
        }

        #[tokio::test]
        async fn admin_save_validates_threshold_ranges() {
            let (state, _dir) = test_state().await;
            let token = login(&state, "admin", "correct-horse-battery").await;
            let router = super::super::router().with_state(state.clone());

            // Renders the admin half of the page (the manager test covers
            // the other branch), so a template slip fails here, not on
            // someone's screen.
            let response = send(
                &router,
                "GET",
                "/settings/notifications",
                Some(&token),
                None,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);

            let response = send(
                &router,
                "POST",
                "/settings/notifications",
                Some(&token),
                Some("provider=none&timeout_s=10&max_attempts=3&battery_threshold=15&storage_threshold=40"),
            )
            .await;
            assert_eq!(
                location(&response),
                "/settings/notifications?err=bad_storage"
            );

            let response = send(
                &router,
                "POST",
                "/settings/notifications",
                Some(&token),
                Some("provider=none&timeout_s=10&max_attempts=3&system_recipients=ops%40example.com&battery_enabled=true&battery_threshold=15&storage_threshold=85"),
            )
            .await;
            assert_eq!(location(&response), "/settings/notifications?ok=saved");
            let saved = state
                .db
                .run(|s| s.get_system_alert_config())
                .await
                .expect("load system config");
            assert_eq!(saved.battery_threshold, 15);
            assert_eq!(saved.storage_threshold, 85);
            assert!(saved.battery_enabled);
            assert!(!saved.network_enabled, "unchecked toggle saves as false");
            assert_eq!(saved.recipients, vec!["ops@example.com"]);
        }

        /// UX-059: the Hub picker names each Hub whose alarms e-mail nobody.
        #[tokio::test]
        async fn the_hub_picker_marks_hubs_without_recipients() {
            use crate::state::IngestEvent;
            use fiber_protocol::payloads::status::DeviceStatus;

            let (state, _dir) = test_state().await;
            let token = login_as_role(&state, Role::Manager, "manager1").await;
            for hostname in ["hub-a", "hub-b"] {
                state.store.apply(IngestEvent::Status {
                    hostname: hostname.into(),
                    status: DeviceStatus::Online,
                });
            }
            let cfg = portal_storage::DeviceReportConfig {
                hostname: "hub-a".into(),
                recipients: vec!["ops@example.com".into()],
                enabled: true,
                notify_on_alarm: true,
                notify_on_clear: false,
                notify_on_ack: false,
                updated_at: None,
            };
            state
                .db
                .run(move |s| s.upsert_device_report_config(&cfg))
                .await
                .unwrap();
            let router = super::super::router().with_state(state);

            let response = send(
                &router,
                "GET",
                "/settings/notifications",
                Some(&token),
                None,
            )
            .await;
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let html = String::from_utf8_lossy(&bytes);
            let option = |host: &str| {
                let start = html
                    .find(&format!(r#"<option value="{host}""#))
                    .unwrap_or_else(|| panic!("no option for {host}: {html}"));
                let end = start + html[start..].find("</option>").unwrap();
                html[start..end].to_string()
            };
            assert!(
                !option("hub-a").contains("no recipients"),
                "{}",
                option("hub-a")
            );
            assert!(
                option("hub-b").contains("no recipients"),
                "{}",
                option("hub-b")
            );
        }

        /// decisions.md #31: a manager sets the default alarm recipients on
        /// this page; the change is audited, the picked Hub gets no row of its
        /// own from a save that left its section empty, and a Hub covered by
        /// the defaults is no longer marked.
        #[tokio::test]
        async fn default_recipients_save_audit_and_cover_hubs_without_a_row() {
            use crate::state::IngestEvent;
            use fiber_protocol::payloads::status::DeviceStatus;

            let (state, _dir) = test_state().await;
            let token = login_as_role(&state, Role::Manager, "manager1").await;
            // Two Hubs, so the page draws the picker with its marks.
            for hostname in ["hub-a", "hub-b"] {
                state.store.apply(IngestEvent::Status {
                    hostname: hostname.into(),
                    status: DeviceStatus::Online,
                });
            }
            let router = super::super::router().with_state(state.clone());

            let response = send(
                &router,
                "POST",
                "/settings/notifications",
                Some(&token),
                Some("hub=hub-b&notify_on_alarm=true&default_recipients=duty%40example.com"),
            )
            .await;
            assert_eq!(
                location(&response),
                "/settings/notifications?ok=saved&hub=hub-b"
            );
            let (defaults, row) = state
                .db
                .run(|s| {
                    Ok((
                        s.default_alarm_recipients()?,
                        s.get_device_report_config("hub-b")?,
                    ))
                })
                .await
                .unwrap();
            assert_eq!(defaults, ["duty@example.com".to_string()]);
            assert!(row.is_none(), "an untouched Hub section writes no row");

            let (audit, _) = state
                .auth
                .audit_page(portal_storage::AuditFilter {
                    event_type: Some("notify_config".into()),
                    limit: 50,
                    ..Default::default()
                })
                .await
                .unwrap();
            assert!(
                audit.iter().any(|a| a
                    .details
                    .as_deref()
                    .is_some_and(|d| d.contains("duty@example.com"))),
                "{audit:?}"
            );

            let response = send(
                &router,
                "GET",
                "/settings/notifications?hub=hub-b",
                Some(&token),
                None,
            )
            .await;
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let html = String::from_utf8_lossy(&bytes);
            assert!(html.contains("duty@example.com"), "defaults are shown");
            assert!(
                html.contains(r#"<option value="hub-b""#),
                "the picker is drawn"
            );
            assert!(!html.contains("· no recipients"), "both Hubs are covered");
            assert!(html.contains("its alarms go to the default recipients"));
        }

        /// MR !103 review: a save for a Hub the registry no longer holds (a
        /// restart that lost its retained messages) is refused. It used to
        /// fall back to the first Hub and write, or delete, that Hub's row.
        #[tokio::test]
        async fn a_save_for_an_unknown_hub_touches_no_other_hub() {
            use crate::state::IngestEvent;
            use fiber_protocol::payloads::status::DeviceStatus;

            let (state, _dir) = test_state().await;
            let token = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "hub-a".into(),
                status: DeviceStatus::Online,
            });
            let router = super::super::router().with_state(state.clone());
            send(
                &router,
                "POST",
                "/settings/notifications",
                Some(&token),
                Some("hub=hub-a&device_enabled=true&device_recipients=a%40example.com"),
            )
            .await;

            for body in [
                "hub=hub-b&device_enabled=true&device_recipients=b%40example.com",
                "hub=hub-b&notify_on_alarm=true",
            ] {
                let response = send(
                    &router,
                    "POST",
                    "/settings/notifications",
                    Some(&token),
                    Some(body),
                )
                .await;
                assert_eq!(
                    location(&response),
                    "/settings/notifications?err=unknown_hub"
                );
            }
            let row = state
                .db
                .run(|s| s.get_device_report_config("hub-a"))
                .await
                .unwrap()
                .expect("hub-a keeps its row");
            assert_eq!(row.recipients, ["a@example.com".to_string()]);
        }

        /// A Hub section emptied on save drops the Hub's own row, so the Hub
        /// falls back to the defaults instead of keeping an empty override.
        #[tokio::test]
        async fn an_emptied_hub_section_falls_back_to_the_defaults() {
            use crate::state::IngestEvent;
            use fiber_protocol::payloads::status::DeviceStatus;

            let (state, _dir) = test_state().await;
            let token = login_as_role(&state, Role::Manager, "manager1").await;
            state.store.apply(IngestEvent::Status {
                hostname: "hub-a".into(),
                status: DeviceStatus::Online,
            });
            let router = super::super::router().with_state(state.clone());
            let saved = send(
                &router,
                "POST",
                "/settings/notifications",
                Some(&token),
                Some("hub=hub-a&device_enabled=true&device_recipients=a%40example.com"),
            )
            .await;
            assert_eq!(saved.status(), StatusCode::SEE_OTHER);
            let row = state
                .db
                .run(|s| s.get_device_report_config("hub-a"))
                .await
                .unwrap();
            assert!(row.is_some());

            send(
                &router,
                "POST",
                "/settings/notifications",
                Some(&token),
                Some("hub=hub-a&notify_on_alarm=true"),
            )
            .await;
            let row = state
                .db
                .run(|s| s.get_device_report_config("hub-a"))
                .await
                .unwrap();
            assert!(row.is_none(), "{row:?}");
        }
    }
}
