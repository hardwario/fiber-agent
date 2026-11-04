//! The calibration card's routes, for Nodes, Beacons and Hub probes alike
//! (plan/portal/calibration.cs.md §2, §4, amendments 2026-10-13 and
//! 2026-10-14, GitLab #99). A probe's prefix is
//! `/hubs/{hub}/sensors/{line}` (line 0..7 as on every other probe route),
//! a Node's `/nodes/{node_key}`, a Beacon's `/beacons/{mac}`:
//!
//! - `POST …/calibrations` — upload a certificate: multipart `file` (PDF) + `valid_until` (the
//!   expiry date, `YYYY-MM-DD`, required) + `note` (optional). Manager and
//!   above.
//! - `POST …/calibrations/date` — move the current certificate's expiry
//!   date without a new PDF ("Upravit datum"). Manager and above.
//! - `POST …/calibrations/{id}/delete` — delete one certificate, current or
//!   older, with a required `message` (the reason, 1–500 characters after
//!   trimming). Admin (and superuser) only (amendment 2026-10-13).
//! - `GET …/calibrations/{id}.pdf` — download, as an attachment. Viewer and
//!   above; a deleted certificate is a 404.
//! - `GET …/calibrations/card` — the card alone, the summary table's modal
//!   body (UI amendment 2026-10-14). Viewer and above, as the page it sits
//!   on; the card itself hides the forms a viewer may not use.
//!
//! Every write answers the card's htmx form with the card itself, re-rendered
//! around its flash (`partials/calibration_card.html`, swapped `outerHTML`
//! into `#calibration-card`) — the jobs-table shape of the backfill form,
//! not `#command-status`: nothing here is a device command, and a refusal
//! is a 200 carrying its message, because htmx does not swap a 4xx.
//!
//! The logic is `crate::calibration`'s; these handlers resolve the device,
//! read the form, audit, and render.

use axum::extract::multipart::MultipartError;
use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use minijinja::context;
use serde::Deserialize;
use serde_json::json;

use crate::auth::{CurrentUser, RequireAdmin, RequireManager, Role};
use crate::calibration::{
    self, CalibrationScope, DeleteError, DeviceKey, Upload, UploadError, MAX_DELETE_MESSAGE_CHARS,
    MAX_NOTE_CHARS, MAX_PDF_BYTES, MAX_PER_DEVICE,
};
use crate::http::AppState;
use crate::render;

use super::{non_empty, render_fragment, tr};

/// The upload route's body cap: the largest PDF plus room for the multipart
/// envelope and the two text fields. The limit is this route's alone —
/// every other route keeps axum's 2 MB default.
const UPLOAD_BODY_LIMIT: usize = MAX_PDF_BYTES + 256 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        // The two downloads first, on their own registrations: they are the
        // only GETs here, and `every_mutating_route_declares_a_role` reads a
        // `post(` anywhere in a registration's span as a mutation.
        //
        // `{file}` rather than `{id}.pdf`: matchit 0.8 has no suffixed
        // parameters, so the handler strips the extension itself. The
        // literal `date` below wins over `{file}` for the POST.
        // The modal's card: a literal segment, which matchit prefers to
        // the `{file}` beside it.
        .route("/nodes/{node_key}/calibrations/card", get(card_node))
        .route("/beacons/{mac}/calibrations/card", get(card_beacon))
        .route(
            "/hubs/{hub}/sensors/{line}/calibrations/card",
            get(card_probe),
        )
        .route("/nodes/{node_key}/calibrations/{file}", get(download_node))
        .route("/beacons/{mac}/calibrations/{file}", get(download_beacon))
        // `{file}` again, not `{id}`: matchit refuses two parameter names
        // in one segment position. Here it is the bare id.
        .route(
            "/nodes/{node_key}/calibrations/{file}/delete",
            post(delete_node),
        )
        .route(
            "/beacons/{mac}/calibrations/{file}/delete",
            post(delete_beacon),
        )
        .route(
            "/nodes/{node_key}/calibrations",
            post(upload_node).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        .route("/nodes/{node_key}/calibrations/date", post(date_node))
        .route(
            "/beacons/{mac}/calibrations",
            post(upload_beacon).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        .route("/beacons/{mac}/calibrations/date", post(date_beacon))
        // A Hub probe (amendment 2026-10-14), in the same order and for the
        // same matchit reasons as above.
        .route(
            "/hubs/{hub}/sensors/{line}/calibrations/{file}",
            get(download_probe),
        )
        .route(
            "/hubs/{hub}/sensors/{line}/calibrations/{file}/delete",
            post(delete_probe),
        )
        .route(
            "/hubs/{hub}/sensors/{line}/calibrations",
            post(upload_probe).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        .route(
            "/hubs/{hub}/sensors/{line}/calibrations/date",
            post(date_probe),
        )
}

/// One device a calibration route acts on: its key and the URL prefix the
/// card's links and forms hang off.
pub(crate) struct Target {
    key: DeviceKey,
    base: String,
    /// A probe's line, for its card's own element id
    /// ([`render::CalibrationCardCtx::card_id`]); `None` for a Node or Beacon.
    line: Option<u8>,
    /// A probe's "1 · Sensor 1" (line from 1 + name or "Line N"), the
    /// summary's Sonda column and the modal's heading.
    title: Option<String>,
}

impl Target {
    /// A Node, through the Hub that actually holds it (devices.hub.md §12 E).
    /// `None` for a key no Hub knows — a 404, like every other Node route.
    pub(crate) fn node(state: &AppState, node_key: &str) -> Option<Self> {
        let node_key = node_key.to_lowercase();
        let snapshot = state.store.snapshot();
        let (device, _) = snapshot.hub_of_node(&node_key)?;
        Some(Self {
            key: DeviceKey::new(&device.hostname, CalibrationScope::Node, &node_key),
            base: format!("/nodes/{node_key}"),
            line: None,
            title: None,
        })
    }

    /// A registered Beacon. A discovered candidate is not a sensor yet
    /// (it waits on `/beacons` to be added), so it has no calibration.
    pub(crate) fn beacon(state: &AppState, mac: &str) -> Option<Self> {
        let mac = mac.trim().to_uppercase();
        let snapshot = state.store.snapshot();
        let (device, beacon) = snapshot.hub_of_beacon(&mac)?;
        if beacon.discovered == Some(true) {
            return None;
        }
        Some(Self {
            key: DeviceKey::new(&device.hostname, CalibrationScope::Beacon, &mac),
            base: format!("/beacons/{mac}"),
            line: None,
            title: None,
        })
    }

    /// One wired line (0..7) of a Hub the live state holds (amendment
    /// 2026-10-14) — the Hub's own spelling of its hostname, the one its
    /// other alarms are stored under. A Hub the state does not hold has no
    /// lines to show, so it is a 404 like `GET …/sensors/{line}/row`.
    pub(crate) fn probe(state: &AppState, hub: &str, line: u8) -> Option<Self> {
        let wanted = hub.trim();
        let snapshot = state.store.snapshot();
        let device = snapshot
            .devices
            .values()
            .find(|d| !wanted.is_empty() && d.hostname.trim().eq_ignore_ascii_case(wanted))?;
        let sensor = device.sensors.get(usize::from(line))?;
        let name = sensor
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                tr("monitoring.shared.line_fallback").replace("{n}", &(line + 1).to_string())
            });
        let hostname = device.hostname.clone();
        Some(Self {
            key: DeviceKey::new(&hostname, CalibrationScope::Probe, &line.to_string()),
            base: format!("/hubs/{hostname}/sensors/{line}"),
            line: Some(line),
            title: Some(format!("{} · {name}", line + 1)),
        })
    }
}

/// The card for one device, as the detail page and every write render it
/// for `user`: manager and above edit, admin (or superuser) also deletes.
/// A read failure degrades to "no calibration" rather than failing the page
/// the card sits on.
pub(crate) async fn card_ctx(
    state: &AppState,
    target: &Target,
    user: &CurrentUser,
) -> render::CalibrationCardCtx {
    let rows = state
        .calibration
        .history(&target.key)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(%err, entity = target.key.entity, "calibrations unreadable; the card renders empty");
            Vec::new()
        });
    let mut cal = render::calibration_card_ctx(
        &target.base,
        &rows,
        &state.calibration.config(),
        time::OffsetDateTime::now_utc().unix_timestamp(),
        user.has_role(Role::Manager),
        user.is_admin(),
    );
    if let Some(line) = target.line {
        cal.card_id = format!("calibration-card-{line}");
    }
    cal.title = target.title.clone();
    cal
}

// ---------------------------------------------------------------------
// GET …/calibrations/card — the modal's body
// ---------------------------------------------------------------------

async fn card_node(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
) -> Response {
    let Some(target) = Target::node(&state, &node_key) else {
        return not_found();
    };
    card_fragment(&state, &user, &target).await
}

async fn card_beacon(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(mac): Path<String>,
) -> Response {
    let Some(target) = Target::beacon(&state, &mac) else {
        return not_found();
    };
    card_fragment(&state, &user, &target).await
}

async fn card_probe(
    user: CurrentUser,
    State(state): State<AppState>,
    Path((hub, line)): Path<(String, u8)>,
) -> Response {
    let Some(target) = Target::probe(&state, &hub, line) else {
        return not_found();
    };
    card_fragment(&state, &user, &target).await
}

async fn card_fragment(state: &AppState, user: &CurrentUser, target: &Target) -> Response {
    let cal = card_ctx(state, target, user).await;
    render_fragment(
        state,
        "partials/calibration_card.html",
        render::page_ctx(context! { cal => cal }, Some(user)),
    )
}

/// A catalogue message with this feature's limits filled in.
fn message(key: &str) -> String {
    tr(key)
        .replace("{mb}", &(MAX_PDF_BYTES / (1024 * 1024)).to_string())
        .replace("{max}", &MAX_PER_DEVICE.to_string())
        .replace("{max_note}", &MAX_NOTE_CHARS.to_string())
        .replace("{max_delete}", &MAX_DELETE_MESSAGE_CHARS.to_string())
}

/// The card, re-rendered with one flash, as `user` may see it — only
/// managers reach the writes, so it is always at least the editable card.
async fn card_with(
    state: &AppState,
    user: &CurrentUser,
    target: &Target,
    flash: Result<String, String>,
) -> Response {
    let mut cal = card_ctx(state, target, user).await;
    match flash {
        Ok(ok) => cal.ok = Some(ok),
        Err(error) => cal.error = Some(error),
    }
    render_fragment(
        state,
        "partials/calibration_card.html",
        render::page_ctx(context! { cal => cal }, Some(user)),
    )
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

// ---------------------------------------------------------------------
// POST …/calibrations
// ---------------------------------------------------------------------

/// The upload form's three fields, as read off the multipart stream.
#[derive(Default)]
struct UploadForm {
    file_name: Option<String>,
    bytes: Vec<u8>,
    valid_until: String,
    note: String,
}

/// The card message for a multipart stream that failed to read. The body
/// cap surfaces here, as the stream's 413, rather than as a bare error page:
/// htmx would not show that at all.
fn multipart_error_key(err: &MultipartError) -> &'static str {
    if err.status() == StatusCode::PAYLOAD_TOO_LARGE {
        "monitoring.calibration.err_too_large"
    } else {
        "monitoring.calibration.err_bad_form"
    }
}

/// Reads the form, stopping at the first byte past [`MAX_PDF_BYTES`]
/// instead of buffering whatever the body cap lets through. Unknown fields
/// are skipped.
async fn read_upload_form(multipart: &mut Multipart) -> Result<UploadForm, &'static str> {
    let mut form = UploadForm::default();
    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(err) => return Err(multipart_error_key(&err)),
        };
        match field.name() {
            Some("file") => {
                form.file_name = field.file_name().map(str::to_string);
                while let Some(chunk) = field.chunk().await.map_err(|e| multipart_error_key(&e))? {
                    if form.bytes.len() + chunk.len() > MAX_PDF_BYTES {
                        return Err("monitoring.calibration.err_too_large");
                    }
                    form.bytes.extend_from_slice(&chunk);
                }
            }
            Some("valid_until") => {
                form.valid_until = field.text().await.map_err(|e| multipart_error_key(&e))?;
            }
            Some("note") => {
                form.note = field.text().await.map_err(|e| multipart_error_key(&e))?;
            }
            _ => {}
        }
    }
    Ok(form)
}

async fn upload_node(
    RequireManager(user): RequireManager,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
    multipart: Multipart,
) -> Response {
    let Some(target) = Target::node(&state, &node_key) else {
        return not_found();
    };
    upload(&state, &user, &target, multipart).await
}

async fn upload_beacon(
    RequireManager(user): RequireManager,
    State(state): State<AppState>,
    Path(mac): Path<String>,
    multipart: Multipart,
) -> Response {
    let Some(target) = Target::beacon(&state, &mac) else {
        return not_found();
    };
    upload(&state, &user, &target, multipart).await
}

async fn upload_probe(
    RequireManager(user): RequireManager,
    State(state): State<AppState>,
    Path((hub, line)): Path<(String, u8)>,
    multipart: Multipart,
) -> Response {
    let Some(target) = Target::probe(&state, &hub, line) else {
        return not_found();
    };
    upload(&state, &user, &target, multipart).await
}

async fn upload(
    state: &AppState,
    user: &CurrentUser,
    target: &Target,
    mut multipart: Multipart,
) -> Response {
    let form = match read_upload_form(&mut multipart).await {
        Ok(form) => form,
        Err(key) => return card_with(state, user, target, Err(message(key))).await,
    };
    let Some(valid_until) = calibration::parse_date(&form.valid_until) else {
        return card_with(
            state,
            user,
            target,
            Err(message("monitoring.calibration.err_date")),
        )
        .await;
    };

    let upload = Upload {
        file_name: form.file_name,
        bytes: form.bytes,
        valid_until,
        note: non_empty(form.note),
    };
    match state
        .calibration
        .upload(&target.key, &user.username, upload)
        .await
    {
        Ok(row) => {
            super::audit_action(
                state,
                user,
                "device_calibration_upload",
                &target.key.entity,
                json!({
                    "hostname": target.key.hostname,
                    "scope": target.key.scope.as_str(),
                    "id": row.id,
                    "file_name": row.file_name,
                    "file_size": row.file_size,
                    "sha256": row.sha256,
                    "valid_until": calibration::format_date(row.valid_until),
                    "note": row.note,
                }),
                true,
            )
            .await;
            card_with(
                state,
                user,
                target,
                Ok(message("monitoring.calibration.uploaded_ok")),
            )
            .await
        }
        Err(err) => {
            // A refused file is the operator's to fix and the card says so;
            // a failed write is ours, and is the one worth a trail.
            if matches!(
                err,
                UploadError::Io(_) | UploadError::Storage(_) | UploadError::BadIdentity
            ) {
                tracing::warn!(%err, entity = target.key.entity, "calibration upload failed");
                super::audit_action(
                    state,
                    user,
                    "device_calibration_upload",
                    &target.key.entity,
                    json!({
                        "hostname": target.key.hostname,
                        "scope": target.key.scope.as_str(),
                        "error": err.to_string(),
                    }),
                    false,
                )
                .await;
            }
            card_with(state, user, target, Err(message(err.message_key()))).await
        }
    }
}

// ---------------------------------------------------------------------
// POST …/calibrations/date
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct DateForm {
    #[serde(default)]
    valid_until: String,
}

async fn date_node(
    RequireManager(user): RequireManager,
    State(state): State<AppState>,
    Path(node_key): Path<String>,
    Form(form): Form<DateForm>,
) -> Response {
    let Some(target) = Target::node(&state, &node_key) else {
        return not_found();
    };
    set_date(&state, &user, &target, &form).await
}

async fn date_beacon(
    RequireManager(user): RequireManager,
    State(state): State<AppState>,
    Path(mac): Path<String>,
    Form(form): Form<DateForm>,
) -> Response {
    let Some(target) = Target::beacon(&state, &mac) else {
        return not_found();
    };
    set_date(&state, &user, &target, &form).await
}

async fn date_probe(
    RequireManager(user): RequireManager,
    State(state): State<AppState>,
    Path((hub, line)): Path<(String, u8)>,
    Form(form): Form<DateForm>,
) -> Response {
    let Some(target) = Target::probe(&state, &hub, line) else {
        return not_found();
    };
    set_date(&state, &user, &target, &form).await
}

async fn set_date(
    state: &AppState,
    user: &CurrentUser,
    target: &Target,
    form: &DateForm,
) -> Response {
    let Some(valid_until) = calibration::parse_date(&form.valid_until) else {
        return card_with(
            state,
            user,
            target,
            Err(message("monitoring.calibration.err_date")),
        )
        .await;
    };
    match state
        .calibration
        .set_valid_until(&target.key, valid_until)
        .await
    {
        Ok(Some((id, previous))) => {
            super::audit_action(
                state,
                user,
                "device_calibration_date_set",
                &target.key.entity,
                json!({
                    "hostname": target.key.hostname,
                    "scope": target.key.scope.as_str(),
                    "id": id,
                    "previous": calibration::format_date(previous),
                    "valid_until": calibration::format_date(valid_until),
                }),
                true,
            )
            .await;
            card_with(
                state,
                user,
                target,
                Ok(message("monitoring.calibration.date_saved")),
            )
            .await
        }
        Ok(None) => {
            card_with(
                state,
                user,
                target,
                Err(message("monitoring.calibration.err_nothing_to_date")),
            )
            .await
        }
        Err(err) => {
            tracing::warn!(%err, entity = target.key.entity, "calibration date change failed");
            super::audit_action(
                state,
                user,
                "device_calibration_date_set",
                &target.key.entity,
                json!({
                    "hostname": target.key.hostname,
                    "scope": target.key.scope.as_str(),
                    "valid_until": calibration::format_date(valid_until),
                    "error": err.to_string(),
                }),
                false,
            )
            .await;
            card_with(
                state,
                user,
                target,
                Err(message("monitoring.calibration.err_save_failed")),
            )
            .await
        }
    }
}

// ---------------------------------------------------------------------
// POST …/calibrations/{id}/delete
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct DeleteForm {
    #[serde(default)]
    message: String,
}

async fn delete_node(
    RequireAdmin(user): RequireAdmin,
    State(state): State<AppState>,
    Path((node_key, id)): Path<(String, String)>,
    Form(form): Form<DeleteForm>,
) -> Response {
    let Some(target) = Target::node(&state, &node_key) else {
        return not_found();
    };
    delete(&state, &user, &target, &id, &form).await
}

async fn delete_beacon(
    RequireAdmin(user): RequireAdmin,
    State(state): State<AppState>,
    Path((mac, id)): Path<(String, String)>,
    Form(form): Form<DeleteForm>,
) -> Response {
    let Some(target) = Target::beacon(&state, &mac) else {
        return not_found();
    };
    delete(&state, &user, &target, &id, &form).await
}

async fn delete_probe(
    RequireAdmin(user): RequireAdmin,
    State(state): State<AppState>,
    Path((hub, line, id)): Path<(String, u8, String)>,
    Form(form): Form<DeleteForm>,
) -> Response {
    let Some(target) = Target::probe(&state, &hub, line) else {
        return not_found();
    };
    delete(&state, &user, &target, &id, &form).await
}

/// A refusal (no reason, an unknown id) comes back on the card like every
/// other one here, as a 200 htmx swaps; only the device itself 404s.
async fn delete(
    state: &AppState,
    user: &CurrentUser,
    target: &Target,
    id: &str,
    form: &DeleteForm,
) -> Response {
    match state
        .calibration
        .delete(&target.key, id, &user.username, &form.message)
        .await
    {
        Ok(deleted) => {
            let row = &deleted.row;
            super::audit_action(
                state,
                user,
                "device_calibration_delete",
                &target.key.entity,
                json!({
                    "hostname": target.key.hostname,
                    "scope": target.key.scope.as_str(),
                    "id": row.id,
                    "file_name": row.file_name,
                    "sha256": row.sha256,
                    "valid_until": calibration::format_date(row.valid_until),
                    "was_current": deleted.was_current,
                    "message": row.delete_message,
                }),
                true,
            )
            .await;
            card_with(
                state,
                user,
                target,
                Ok(message("monitoring.calibration.deleted_ok")),
            )
            .await
        }
        Err(err) => {
            if matches!(err, DeleteError::Io(_) | DeleteError::Storage(_)) {
                tracing::warn!(%err, entity = target.key.entity, "calibration delete failed");
                super::audit_action(
                    state,
                    user,
                    "device_calibration_delete",
                    &target.key.entity,
                    json!({
                        "hostname": target.key.hostname,
                        "scope": target.key.scope.as_str(),
                        "id": id,
                        "message": form.message.trim(),
                        "error": err.to_string(),
                    }),
                    false,
                )
                .await;
            }
            card_with(state, user, target, Err(message(err.message_key()))).await
        }
    }
}

// ---------------------------------------------------------------------
// GET …/calibrations/{id}.pdf
// ---------------------------------------------------------------------

async fn download_node(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path((node_key, file)): Path<(String, String)>,
) -> Response {
    let Some(target) = Target::node(&state, &node_key) else {
        return not_found();
    };
    download(&state, &target, &file).await
}

async fn download_beacon(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path((mac, file)): Path<(String, String)>,
) -> Response {
    let Some(target) = Target::beacon(&state, &mac) else {
        return not_found();
    };
    download(&state, &target, &file).await
}

async fn download_probe(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path((hub, line, file)): Path<(String, u8, String)>,
) -> Response {
    let Some(target) = Target::probe(&state, &hub, line) else {
        return not_found();
    };
    download(&state, &target, &file).await
}

/// `attachment` with the uploaded name twice: a plain-ASCII fallback, and
/// the exact UTF-8 name in RFC 6266's `filename*` for the browsers that read
/// it (every current one) — a Czech lab's "Kalibrační list.pdf" survives.
fn content_disposition(file_name: &str) -> String {
    let ascii: String = file_name
        .chars()
        .map(|c| {
            if (c.is_ascii_graphic() && c != '"' && c != '\\') || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{}",
        super::percent_encode_query(file_name)
    )
}

async fn download(state: &AppState, target: &Target, file: &str) -> Response {
    let Some(id) = file.strip_suffix(".pdf") else {
        return not_found();
    };
    let Some((row, bytes)) = state.calibration.certificate(&target.key, id).await else {
        return not_found();
    };
    let Ok(disposition) = HeaderValue::from_str(&content_disposition(&row.file_name)) else {
        return not_found();
    };
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/pdf"),
            ),
            (header::CONTENT_DISPOSITION, disposition),
            // The bytes are an operator's upload: never let a browser sniff
            // them into something it would render as a page of this origin.
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, no-store"),
            ),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    use crate::auth::{Role, SESSION_COOKIE};
    use crate::http::AppState;
    use crate::routes::test_support::{login, login_as_role, test_state};
    use crate::state::events::{IngestEvent, NodeReadingMsg};

    const NODE: &str = "70b3d5e050011234";
    const MAC: &str = "AA:BB:CC:DD:EE:01";
    const PDF: &[u8] = b"%PDF-1.4\n1 0 obj << >> endobj\ntrailer << >>\n%%EOF\n";
    const BOUNDARY: &str = "proximos-calibration-test";
    const DAY: i64 = 86_400;

    fn seed_node(state: &AppState) {
        state.store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: NODE.into(),
            payload: Box::new(NodeReadingMsg {
                page_count: None,
                page_index: None,
                message_id: "m-1".into(),
                dev_eui: NODE.into(),
                serial: None,
                fields: [("temperature".to_string(), 4.2)].into_iter().collect(),
                counters: Default::default(),
                events: Vec::new(),
                gateways: Vec::new(),
                rssi: None,
                snr: None,
                fport: None,
                dr: None,
                received_at: "2026-09-25T10:00:00Z".to_string(),
            }),
        });
    }

    fn seed_beacon(state: &AppState, mac: &str, discovered: Option<bool>) {
        let tag = fiber_protocol::payloads::eye::EyeTag {
            ble_config: None,
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
            discovered,
            field_alarm_states: std::collections::HashMap::new(),
            alarm_state: None,
        };
        state.store.apply(IngestEvent::EyeSensors {
            hostname: "h1".into(),
            payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                timestamp: None,
                discovered_capped: false,
                tags: vec![tag],
            },
        });
    }

    /// The node, beacon and calibration routers together: the detail pages
    /// and the lists render the card and the icon, the calibration routes
    /// write.
    fn app(state: &AppState) -> axum::Router {
        crate::routes::node_routes::router()
            .merge(crate::routes::beacon_routes::router())
            .merge(crate::routes::hub_routes::router())
            .merge(crate::routes::probe_routes::router())
            .merge(super::router())
            .with_state(state.clone())
    }

    /// A multipart body by hand: `(name, file name, bytes)` per part.
    fn multipart(parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, file_name, value) in parts {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
            match file_name {
                Some(file) => body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"{file}\"\r\n\
                         Content-Type: application/pdf\r\n\r\n"
                    )
                    .as_bytes(),
                ),
                None => body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                ),
            }
            body.extend_from_slice(value);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        body
    }

    fn upload_body(file: &[u8], valid_until: &str) -> Vec<u8> {
        multipart(&[
            ("file", Some("Kalibrační list.pdf"), file),
            ("valid_until", None, valid_until.as_bytes()),
            ("note", None, "lab 42".as_bytes()),
        ])
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        token: &str,
        content_type: Option<String>,
        body: Vec<u8>,
    ) -> axum::response::Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
            .header("HX-Request", "true");
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        app.clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .expect("response")
    }

    async fn upload(
        app: &axum::Router,
        path: &str,
        token: &str,
        body: Vec<u8>,
    ) -> axum::response::Response {
        send(
            app,
            "POST",
            path,
            token,
            Some(format!("multipart/form-data; boundary={BOUNDARY}")),
            body,
        )
        .await
    }

    async fn body_string(response: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    async fn rows(
        state: &AppState,
        scope: &str,
        entity: &str,
    ) -> Vec<portal_storage::DeviceCalibration> {
        let (scope, entity) = (scope.to_string(), entity.to_string());
        state
            .db
            .run(move |s| s.list_device_calibrations("h1", &scope, &entity))
            .await
            .expect("list")
    }

    async fn audit(state: &AppState, event_type: &str) -> Vec<portal_storage::AuditEntry> {
        state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some(event_type.into()),
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
    }

    fn date_in(days: i64) -> String {
        crate::calibration::format_date(
            time::OffsetDateTime::now_utc().unix_timestamp() + days * DAY,
        )
    }

    /// §2/§4 end to end: a manager's upload lands as a row and a file, the
    /// card comes back with it, the audit names it, and a viewer downloads
    /// exactly the bytes, as an attachment.
    #[tokio::test]
    async fn a_manager_uploads_a_certificate_and_a_viewer_downloads_it() {
        let (state, _dir) = test_state().await;
        seed_node(&state);
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
        let app = app(&state);

        let response = upload(
            &app,
            &format!("/nodes/{NODE}/calibrations"),
            &manager,
            upload_body(PDF, &date_in(200)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = body_string(response).await;
        assert!(
            html.contains(r#"id="calibration-card""#),
            "the card swaps back"
        );
        assert!(html.contains("Calibration uploaded."), "{html}");
        assert!(html.contains("Kalibrační list.pdf"));
        assert!(html.contains("Valid"), "the state badge");
        // The expiry date edits inline (ui_components.md §1).
        assert!(html.contains("te-pencil"), "{html}");
        assert!(
            html.contains(r#"type="date" name="valid_until" id="calibration-card-valid-until" data-te-in required"#),
            "{html}"
        );

        let stored = rows(&state, "node", NODE).await;
        assert_eq!(stored.len(), 1);
        let row = &stored[0];
        assert_eq!(row.file_name, "Kalibrační list.pdf");
        assert_eq!(row.file_size, PDF.len() as i64);
        assert_eq!(row.note.as_deref(), Some("lab 42"));
        assert_eq!(row.uploaded_by, "manager1");
        assert_eq!(
            crate::calibration::format_date(row.valid_until),
            date_in(200)
        );

        let trail = audit(&state, "device_calibration_upload").await;
        assert_eq!(trail.len(), 1);
        assert!(trail[0].success);
        assert_eq!(trail[0].target.as_deref(), Some(NODE));
        let details: serde_json::Value =
            serde_json::from_str(trail[0].details.as_deref().expect("details")).expect("json");
        assert_eq!(details["id"], row.id.as_str());
        assert_eq!(details["sha256"], row.sha256.as_str());

        let response = send(
            &app,
            "GET",
            &format!("/nodes/{NODE}/calibrations/{}.pdf", row.id),
            &viewer,
            None,
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/pdf"
        );
        let disposition = response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(disposition.starts_with("attachment;"), "{disposition}");
        assert!(
            disposition.contains("filename*=UTF-8''Kalibra%C4%8Dn%C3%AD%20list.pdf"),
            "{disposition}"
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], PDF);

        // The detail page carries the card, read-only for the viewer.
        let page = body_string(
            send(
                &app,
                "GET",
                &format!("/nodes/{NODE}/overview"),
                &viewer,
                None,
                Vec::new(),
            )
            .await,
        )
        .await
        // minijinja escapes `/` inside attributes.
        .replace("&#x2f;", "/");
        // Amendment 2026-10-15: the card sits inline on the Overview,
        // the summary table and its modal are gone.
        assert!(page.contains(r#"id="calibration-card""#), "{page}");
        assert!(page.contains(r#"data-calibration-card="valid""#), "{page}");
        assert!(!page.contains("calibration-modal"));
        assert!(!page.contains(r#"hx-encoding="multipart/form-data""#));
        let page = body_string(
            send(
                &app,
                "GET",
                &format!("/nodes/{NODE}/calibrations/card"),
                &viewer,
                None,
                Vec::new(),
            )
            .await,
        )
        .await
        .replace("&#x2f;", "/");
        assert!(page.contains(r#"id="calibration-card""#));
        assert!(page.contains(&format!("/nodes/{NODE}/calibrations/{}.pdf", row.id)));
        assert!(
            !page.contains(r#"hx-encoding="multipart/form-data""#),
            "a viewer gets no upload form"
        );
    }

    /// The refusals come back on the card (a 200: htmx swaps nothing else),
    /// and none of them leaves a row, a file or an audit entry.
    #[tokio::test]
    async fn a_non_pdf_an_oversized_file_and_a_bad_date_are_refused_on_the_card() {
        let (state, _dir) = test_state().await;
        seed_node(&state);
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let app = app(&state);
        let path = format!("/nodes/{NODE}/calibrations");

        let mut too_large = PDF.to_vec();
        too_large.resize(crate::calibration::MAX_PDF_BYTES + 1, b' ');
        // Past the route's body cap altogether: the multipart stream itself
        // gives up, and that must read the same to the operator.
        let mut past_the_cap = PDF.to_vec();
        past_the_cap.resize(super::UPLOAD_BODY_LIMIT + 1024, b' ');

        for (body, want) in [
            (
                upload_body(b"GIF89a not a certificate", &date_in(90)),
                "The file is not a PDF.",
            ),
            (
                upload_body(&too_large, &date_in(90)),
                "The file is too large; at most 10 MB.",
            ),
            (
                upload_body(&past_the_cap, &date_in(90)),
                "The file is too large; at most 10 MB.",
            ),
            (upload_body(PDF, "31. 12. 2026"), "Enter a valid"),
            (upload_body(PDF, ""), "Enter a valid"),
            (
                multipart(&[("valid_until", None, date_in(90).as_bytes())]),
                "Choose the certificate PDF file.",
            ),
        ] {
            let response = upload(&app, &path, &manager, body).await;
            assert_eq!(response.status(), StatusCode::OK);
            let html = body_string(response).await;
            assert!(html.contains("alert-error"), "{want}: {html}");
            assert!(html.contains(want), "{want}: {html}");
        }

        assert!(rows(&state, "node", NODE).await.is_empty());
        assert!(audit(&state, "device_calibration_upload").await.is_empty());
    }

    /// Writes are manager+, reads viewer+; the role gate runs before the
    /// handler, so a refused write leaves nothing behind.
    #[tokio::test]
    async fn a_viewer_can_neither_upload_nor_change_the_date() {
        let (state, _dir) = test_state().await;
        seed_node(&state);
        let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
        let app = app(&state);

        let response = upload(
            &app,
            &format!("/nodes/{NODE}/calibrations"),
            &viewer,
            upload_body(PDF, &date_in(90)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let response = send(
            &app,
            "POST",
            &format!("/nodes/{NODE}/calibrations/date"),
            &viewer,
            Some("application/x-www-form-urlencoded".into()),
            format!("valid_until={}", date_in(90)).into_bytes(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        assert!(rows(&state, "node", NODE).await.is_empty());
    }

    /// "Upravit datum" moves the current certificate and says what it moved
    /// from in the audit row.
    #[tokio::test]
    async fn a_date_change_moves_the_current_certificate_and_is_audited() {
        let (state, _dir) = test_state().await;
        seed_node(&state);
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let app = app(&state);
        let date_path = format!("/nodes/{NODE}/calibrations/date");
        let form = |date: String| format!("valid_until={date}").into_bytes();

        // Nothing to move yet.
        let html = body_string(
            send(
                &app,
                "POST",
                &date_path,
                &manager,
                Some("application/x-www-form-urlencoded".into()),
                form(date_in(90)),
            )
            .await,
        )
        .await;
        assert!(html.contains("no calibration uploaded"), "{html}");

        upload(
            &app,
            &format!("/nodes/{NODE}/calibrations"),
            &manager,
            upload_body(PDF, &date_in(20)),
        )
        .await;
        let response = send(
            &app,
            "POST",
            &date_path,
            &manager,
            Some("application/x-www-form-urlencoded".into()),
            form(date_in(120)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_string(response).await.contains("Expiry date saved."));

        let stored = rows(&state, "node", NODE).await;
        assert_eq!(
            crate::calibration::format_date(stored[0].valid_until),
            date_in(120)
        );
        let trail = audit(&state, "device_calibration_date_set").await;
        assert_eq!(trail.len(), 1);
        let details: serde_json::Value =
            serde_json::from_str(trail[0].details.as_deref().expect("details")).expect("json");
        assert_eq!(details["previous"], date_in(20).as_str());
        assert_eq!(details["valid_until"], date_in(120).as_str());
    }

    /// A device no Hub knows, an id that is not one, another device's id,
    /// and a Beacon candidate all answer 404.
    #[tokio::test]
    async fn unknown_devices_and_foreign_ids_are_not_found() {
        let (state, _dir) = test_state().await;
        seed_node(&state);
        seed_beacon(&state, MAC, None);
        seed_beacon(&state, "AA:BB:CC:DD:EE:99", Some(true));
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let app = app(&state);

        let response = upload(
            &app,
            "/nodes/70b3d5e05001ffff/calibrations",
            &manager,
            upload_body(PDF, &date_in(90)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = upload(
            &app,
            "/beacons/AA:BB:CC:DD:EE:99/calibrations",
            &manager,
            upload_body(PDF, &date_in(90)),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "a candidate is not a sensor yet"
        );

        // A Beacon's certificate, then asked for through the Node's URL.
        let response = upload(
            &app,
            &format!("/beacons/{}/calibrations", MAC.to_lowercase()),
            &manager,
            upload_body(PDF, &date_in(90)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let beacon_row = rows(&state, "eye", MAC).await.remove(0);

        for path in [
            format!("/nodes/{NODE}/calibrations/{}.pdf", beacon_row.id),
            format!("/nodes/{NODE}/calibrations/{}", beacon_row.id),
            format!("/nodes/{NODE}/calibrations/..%2F..%2Fportal.db.pdf"),
            format!("/beacons/{MAC}/calibrations/{}.txt", beacon_row.id),
        ] {
            let response = send(&app, "GET", &path, &manager, None, Vec::new()).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
        let response = send(
            &app,
            "GET",
            &format!("/beacons/{MAC}/calibrations/{}.pdf", beacon_row.id),
            &manager,
            None,
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// §3 and the 2026-09-25 decision: a lapsed certificate opens a critical
    /// alarm, shows its icon on the list — and is not a fault, on the row or
    /// in the Faults tile, and leaves the device's value state alone.
    #[tokio::test]
    async fn a_lapsed_calibration_alarms_but_is_not_counted_as_a_fault() {
        let (state, _dir) = test_state().await;
        seed_node(&state);
        seed_beacon(&state, MAC, None);
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let app = app(&state);

        for path in [
            format!("/nodes/{NODE}/calibrations"),
            format!("/beacons/{MAC}/calibrations"),
        ] {
            let response = upload(&app, &path, &manager, upload_body(PDF, &date_in(-3))).await;
            assert!(body_string(response).await.contains("Expired"));
        }

        let (open, _) = state
            .alarms
            .list(portal_storage::AlarmListFilter {
                include_cleared: false,
                ..Default::default()
            })
            .await
            .expect("alarms");
        assert_eq!(open.len(), 2, "one alarm per device");
        assert!(open
            .iter()
            .all(|a| a.alarm_type == "CALIBRATION" && a.severity == "critical"));

        let snapshot = state.store.snapshot();
        let liveness = state.offline.liveness_snapshot();
        for scope in [
            crate::render::StatsScope::Nodes,
            crate::render::StatsScope::Beacons,
            crate::render::StatsScope::Fleet,
        ] {
            let stats = crate::render::page_stats_ctx(
                &snapshot,
                &liveness,
                state.alarms.hub_faults(),
                scope,
            );
            let faults = stats
                .tiles
                .iter()
                .find(|t| t.label == "common.metrics.faults")
                .expect("a faults tile");
            assert_eq!(faults.value, 0, "{scope:?}: a certificate is not a fault");
        }

        let rows = crate::routes::node_routes::node_row_data(&state, "h1").await;
        let device = snapshot.device("h1").expect("hub");
        let node = device.nodes.get(NODE).expect("node");
        let ctx = crate::render::node_card_ctx(rows.inputs(node, device));
        assert_eq!(ctx.calibration, Some("expired"));
        assert_eq!(ctx.fault, None);
        // The value and connectivity axes read exactly as without it.
        let without = crate::render::node_card_ctx(crate::render::NodeCardInputs {
            calibration: None,
            ..rows.inputs(node, device)
        });
        assert_eq!(
            (ctx.alarm_state.as_str(), ctx.connected),
            (without.alarm_state.as_str(), without.connected)
        );

        let list = body_string(send(&app, "GET", "/nodes", &manager, None, Vec::new()).await).await;
        assert!(list.contains("Calibration: Expired"), "the list icon");
        assert!(!list.contains("data-fault="), "no fault triangle");
        let list =
            body_string(send(&app, "GET", "/beacons", &manager, None, Vec::new()).await).await;
        assert!(
            list.contains("Calibration: Expired"),
            "the Beacon list icon"
        );
    }

    fn form(body: String) -> (Option<String>, Vec<u8>) {
        (
            Some("application/x-www-form-urlencoded".into()),
            body.into_bytes(),
        )
    }

    async fn open_alarms(state: &AppState) -> Vec<portal_storage::AlarmEvent> {
        state
            .alarms
            .list(portal_storage::AlarmListFilter {
                include_cleared: false,
                include_acknowledged: true,
                alarm_type: Some("CALIBRATION".into()),
                ..Default::default()
            })
            .await
            .expect("alarms")
            .0
    }

    /// Amendment 2026-10-13: deleting is admin work. A manager and a viewer
    /// are refused before the handler, and nothing is deleted.
    #[tokio::test]
    async fn only_an_admin_may_delete_a_certificate() {
        let (state, _dir) = test_state().await;
        seed_node(&state);
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
        let app = app(&state);
        upload(
            &app,
            &format!("/nodes/{NODE}/calibrations"),
            &manager,
            upload_body(PDF, &date_in(90)),
        )
        .await;
        let row = rows(&state, "node", NODE).await.remove(0);

        for token in [&manager, &viewer] {
            let (ct, body) = form("message=oops".into());
            let response = send(
                &app,
                "POST",
                &format!("/nodes/{NODE}/calibrations/{}/delete", row.id),
                token,
                ct,
                body,
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        let after = rows(&state, "node", NODE).await;
        assert!(!after[0].is_deleted());
        assert!(audit(&state, "device_calibration_delete").await.is_empty());

        // The card a manager sees has no delete form; an admin's has one.
        let page = body_string(
            send(
                &app,
                "GET",
                &format!("/nodes/{NODE}/calibrations/card"),
                &manager,
                None,
                Vec::new(),
            )
            .await,
        )
        .await
        .replace("&#x2f;", "/");
        assert!(!page.contains("/delete\""), "no delete form for a manager");
        let admin = login_as_role(&state, Role::Admin, "admin1").await;
        let page = body_string(
            send(
                &app,
                "GET",
                &format!("/nodes/{NODE}/calibrations/card"),
                &admin,
                None,
                Vec::new(),
            )
            .await,
        )
        .await
        .replace("&#x2f;", "/");
        assert!(
            page.contains(&format!("/nodes/{NODE}/calibrations/{}/delete", row.id)),
            "the admin gets the form"
        );
        assert!(page.contains(r#"name="message" required"#));
    }

    /// The whole delete over HTTP: no reason, no delete; then an admin's
    /// delete removes the file, keeps the row marked, closes the alarm the
    /// certificate held, 404s its download, lists it as deleted and audits
    /// the reason.
    #[tokio::test]
    async fn an_admin_deletes_a_certificate_with_a_reason() {
        let (state, dir) = test_state().await;
        seed_node(&state);
        let admin = login_as_role(&state, Role::Admin, "admin1").await;
        let app = app(&state);
        upload(
            &app,
            &format!("/nodes/{NODE}/calibrations"),
            &admin,
            upload_body(PDF, &date_in(-3)),
        )
        .await;
        let row = rows(&state, "node", NODE).await.remove(0);
        assert_eq!(
            open_alarms(&state).await.len(),
            1,
            "an expired upload alarms"
        );
        let file = dir
            .path()
            .join("calibrations")
            .join("node")
            .join(NODE)
            .join(format!("{}.pdf", row.id));
        assert!(file.is_file());
        let delete_path = format!("/nodes/{NODE}/calibrations/{}/delete", row.id);

        for body in ["message=", "message=+++%0A", "other=x"] {
            let (ct, bytes) = form(body.into());
            let response = send(&app, "POST", &delete_path, &admin, ct, bytes).await;
            assert_eq!(response.status(), StatusCode::OK);
            let html = body_string(response).await;
            assert!(html.contains("alert-error"), "{body}: {html}");
            assert!(
                html.contains("Enter the reason for the deletion."),
                "{body}"
            );
        }
        let (ct, bytes) = form(format!("message={}", "x".repeat(501)));
        let html = body_string(send(&app, "POST", &delete_path, &admin, ct, bytes).await).await;
        assert!(html.contains("at most 500 characters"), "{html}");
        assert!(file.is_file(), "nothing deleted yet");
        assert!(!rows(&state, "node", NODE).await[0].is_deleted());
        assert!(audit(&state, "device_calibration_delete").await.is_empty());

        let (ct, bytes) = form("message=++uploaded+to+the+wrong+Node++".into());
        let response = send(&app, "POST", &delete_path, &admin, ct, bytes).await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = body_string(response).await;
        assert!(html.contains("Certificate deleted."), "{html}");
        assert!(
            html.contains("No calibration uploaded."),
            "nothing standing"
        );
        assert!(html.contains("data-deleted"), "listed as deleted");
        assert!(html.contains("uploaded to the wrong Node"));
        assert!(
            !html
                .replace("&#x2f;", "/")
                .contains(&format!("{}.pdf", row.id)),
            "no download link for it"
        );

        assert!(!file.exists(), "the PDF is gone");
        let stored = rows(&state, "node", NODE).await;
        assert_eq!(stored.len(), 1, "the row stays");
        assert!(stored[0].is_deleted());
        assert_eq!(stored[0].deleted_by.as_deref(), Some("admin1"));
        assert_eq!(
            stored[0].delete_message.as_deref(),
            Some("uploaded to the wrong Node")
        );
        assert!(open_alarms(&state).await.is_empty(), "the alarm closed");

        let response = send(
            &app,
            "GET",
            &format!("/nodes/{NODE}/calibrations/{}.pdf", row.id),
            &admin,
            None,
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let trail = audit(&state, "device_calibration_delete").await;
        assert_eq!(trail.len(), 1);
        assert!(trail[0].success);
        assert_eq!(trail[0].target.as_deref(), Some(NODE));
        let details: serde_json::Value =
            serde_json::from_str(trail[0].details.as_deref().expect("details")).expect("json");
        assert_eq!(details["id"], row.id.as_str());
        assert_eq!(details["message"], "uploaded to the wrong Node");
        assert_eq!(details["was_current"], true);

        // A second delete of the same certificate is refused on the card.
        let (ct, bytes) = form("message=again".into());
        let html = body_string(send(&app, "POST", &delete_path, &admin, ct, bytes).await).await;
        assert!(
            html.contains("not found, or it is deleted already"),
            "{html}"
        );
        assert_eq!(audit(&state, "device_calibration_delete").await.len(), 1);
    }

    /// The superuser counts as an admin, and a Beacon's certificate is
    /// deleted through the Beacon's URL; the previous certificate standing
    /// becomes current again.
    #[tokio::test]
    async fn the_superuser_deletes_a_beacon_certificate_and_the_previous_one_returns() {
        let (state, _dir) = test_state().await;
        seed_beacon(&state, MAC, None);
        let root = login(&state, "admin", "correct-horse-battery").await;
        let app = app(&state);
        for days in [200, 10] {
            upload(
                &app,
                &format!("/beacons/{MAC}/calibrations"),
                &root,
                upload_body(PDF, &date_in(days)),
            )
            .await;
        }
        let newest = rows(&state, "eye", MAC).await.remove(0);
        assert_eq!(open_alarms(&state).await.len(), 1, "10 days out warns");

        let (ct, bytes) = form("message=wrong+date".into());
        let response = send(
            &app,
            "POST",
            &format!("/beacons/{MAC}/calibrations/{}/delete", newest.id),
            &root,
            ct,
            bytes,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = body_string(response).await;
        assert!(html.contains("Certificate deleted."), "{html}");
        assert!(html.contains("Valid"), "the older certificate is current");
        assert!(open_alarms(&state).await.is_empty());

        // The modal's card for a Beacon (UI amendment 2026-10-14).
        let card = send(
            &app,
            "GET",
            &format!("/beacons/{MAC}/calibrations/card"),
            &root,
            None,
            Vec::new(),
        )
        .await;
        assert_eq!(card.status(), StatusCode::OK);
        let card = body_string(card).await;
        assert!(card.contains(r#"id="calibration-card""#), "{card}");
        let page = body_string(
            send(
                &app,
                "GET",
                &format!("/beacons/{MAC}/overview"),
                &root,
                None,
                Vec::new(),
            )
            .await,
        )
        .await;
        assert!(page.contains(r#"data-calibration-card="valid""#), "{page}");
        assert!(page.contains(r#"hx-encoding="multipart/form-data""#));
    }

    // ---- Hub probes (amendment 2026-10-14) -------------------------------

    /// Line 2 on the wire, "3" in the UI.
    const LINE: u8 = 2;

    fn probe_path(line: u8) -> String {
        format!("/hubs/h1/sensors/{line}/calibrations")
    }

    /// A probe's card end to end: the upload lands under `probe` + the wire
    /// line of its Hub, the file sits under the Hub's own directory, the
    /// card answers with its own id, the Sensors tab lists one card per
    /// line, and a viewer downloads the bytes.
    #[tokio::test]
    async fn a_manager_uploads_a_probe_certificate_and_a_viewer_downloads_it() {
        let (state, dir) = test_state().await;
        seed_node(&state); // brings Hub h1 and its eight lines into the state
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
        let app = app(&state);

        let response = upload(
            &app,
            &probe_path(LINE),
            &manager,
            upload_body(PDF, &date_in(200)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = body_string(response).await;
        assert!(html.contains(r#"id="calibration-card-2""#), "{html}");
        assert!(
            html.contains("hx-target=\"#calibration-card-2\""),
            "its forms target it"
        );
        assert!(html.contains("Calibration uploaded."), "{html}");

        let stored = rows(&state, "probe", "2").await;
        assert_eq!(stored.len(), 1);
        let row = &stored[0];
        assert_eq!(row.hostname, "h1");
        let file = dir
            .path()
            .join("calibrations")
            .join("probe")
            .join("h1")
            .join("2")
            .join(format!("{}.pdf", row.id));
        assert!(file.exists(), "{file:?}");

        let trail = audit(&state, "device_calibration_upload").await;
        assert_eq!(trail.len(), 1);
        let details: serde_json::Value =
            serde_json::from_str(trail[0].details.as_deref().expect("details")).expect("json");
        assert_eq!(details["scope"], "probe");
        assert_eq!(details["hostname"], "h1");

        let response = send(
            &app,
            "GET",
            &format!("{}/{}.pdf", probe_path(LINE), row.id),
            &viewer,
            None,
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], PDF);

        // Another line of the same Hub holds nothing.
        assert!(rows(&state, "probe", "3").await.is_empty());

        // devices.hub.md §3.2 amendment 2026-10-14: the card is on each
        // probe's own page.
        for line in 0..8 {
            let path = format!("/hubs/h1/sensors/line-{}", line + 1);
            let resp = send(&app, "GET", &path, &viewer, None, Vec::new()).await;
            let status = resp.status();
            let page = body_string(resp).await.replace("&#x2f;", "/");
            assert!(
                page.contains(&format!("calibration-card-{line}")),
                "card for line {line}: {status}"
            );
        }
        let card = body_string(
            send(
                &app,
                "GET",
                &format!("{}/card", probe_path(LINE)),
                &viewer,
                None,
                Vec::new(),
            )
            .await,
        )
        .await
        .replace("&#x2f;", "/");
        assert!(card.contains(&format!(r#"id="calibration-card-{LINE}""#)));
        assert!(card.contains(&format!("{}/{}.pdf", probe_path(LINE), row.id)));
        assert!(
            card.contains(&format!("{} · ", LINE + 1)),
            "the probe's title: {card}"
        );
        assert!(
            !card.contains(r#"hx-encoding="multipart/form-data""#),
            "a viewer gets no upload form"
        );
        let missing = send(
            &app,
            "GET",
            "/hubs/h1/sensors/8/calibrations/card",
            &viewer,
            None,
            Vec::new(),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let page =
            body_string(send(&app, "GET", "/hubs/h1/overview", &viewer, None, Vec::new()).await)
                .await;
        assert!(
            !page.contains("data-calibration-card"),
            "the card is the probe page's"
        );
    }

    /// The probe routes resolve the Hub and the line like every other probe
    /// route: an unknown Hub or a line past 7 is a 404, and a viewer is
    /// refused the writes.
    #[tokio::test]
    async fn unknown_hubs_and_lines_are_not_found_and_viewers_cannot_write() {
        let (state, _dir) = test_state().await;
        seed_node(&state);
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let viewer = login_as_role(&state, Role::Viewer, "viewer1").await;
        let app = app(&state);

        for path in [
            "/hubs/nope/sensors/0/calibrations",
            "/hubs/h1/sensors/8/calibrations",
        ] {
            let response = upload(&app, path, &manager, upload_body(PDF, &date_in(90))).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
        let response = upload(
            &app,
            &probe_path(0),
            &viewer,
            upload_body(PDF, &date_in(90)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let (ct, body) = form(format!("valid_until={}", date_in(90)));
        let response = send(
            &app,
            "POST",
            &format!("{}/date", probe_path(0)),
            &viewer,
            ct,
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(rows(&state, "probe", "0").await.is_empty());
    }

    /// A lapsed probe certificate opens a critical `cal:probe:{line}` alarm
    /// on its Hub, named after the line, draws the icon on the line's row —
    /// and is not a fault. A date moved out of the window closes it.
    #[tokio::test]
    async fn a_lapsed_probe_calibration_alarms_but_is_not_a_fault() {
        let (state, _dir) = test_state().await;
        seed_node(&state);
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let app = app(&state);

        let response = upload(
            &app,
            &probe_path(LINE),
            &manager,
            upload_body(PDF, &date_in(-3)),
        )
        .await;
        assert!(body_string(response).await.contains("Expired"));

        let open = open_alarms(&state).await;
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].hostname, "h1");
        assert_eq!(open[0].sensor_id, "cal:probe:2");
        assert_eq!(open[0].severity, "critical");
        assert_eq!(open[0].sensor_name.as_deref(), Some("Line 3"));

        let snapshot = state.store.snapshot();
        let stats = crate::render::page_stats_ctx(
            &snapshot,
            &state.offline.liveness_snapshot(),
            state.alarms.hub_faults(),
            crate::render::StatsScope::Fleet,
        );
        let faults = stats
            .tiles
            .iter()
            .find(|t| t.label == "common.metrics.faults")
            .expect("a faults tile");
        assert_eq!(faults.value, 0, "a certificate is not a fault");

        let row = body_string(
            send(
                &app,
                "GET",
                "/hubs/h1/sensors/2/row",
                &manager,
                None,
                Vec::new(),
            )
            .await,
        )
        .await;
        assert!(row.contains("Calibration: Expired"), "the row icon: {row}");
        let other = body_string(
            send(
                &app,
                "GET",
                "/hubs/h1/sensors/3/row",
                &manager,
                None,
                Vec::new(),
            )
            .await,
        )
        .await;
        assert!(
            !other.contains("Calibration: Expired"),
            "a line without one draws no icon: {other}"
        );

        let (ct, body) = form(format!("valid_until={}", date_in(200)));
        let response = send(
            &app,
            "POST",
            &format!("{}/date", probe_path(LINE)),
            &manager,
            ct,
            body,
        )
        .await;
        assert!(body_string(response).await.contains("Valid"));
        assert!(open_alarms(&state).await.is_empty(), "the date closed it");
    }

    /// Only an admin deletes a probe certificate, with a reason; the alarm
    /// it held closes and its download 404s.
    #[tokio::test]
    async fn an_admin_deletes_a_probe_certificate_with_a_reason() {
        let (state, _dir) = test_state().await;
        seed_node(&state);
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let admin = login_as_role(&state, Role::Admin, "admin1").await;
        let app = app(&state);
        upload(
            &app,
            &probe_path(LINE),
            &manager,
            upload_body(PDF, &date_in(5)),
        )
        .await;
        let row = rows(&state, "probe", "2").await.remove(0);
        assert_eq!(
            open_alarms(&state).await.len(),
            1,
            "inside the warning window"
        );
        let delete = format!("{}/{}/delete", probe_path(LINE), row.id);

        let (ct, body) = form("message=oops".into());
        let response = send(&app, "POST", &delete, &manager, ct, body).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let (ct, body) = form("message=".into());
        let response = send(&app, "POST", &delete, &admin, ct, body).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !rows(&state, "probe", "2").await[0].is_deleted(),
            "no reason, no delete"
        );

        let (ct, body) = form("message=probe+replaced".into());
        let response = send(&app, "POST", &delete, &admin, ct, body).await;
        let html = body_string(response).await;
        assert!(html.contains(r#"id="calibration-card-2""#));
        let after = rows(&state, "probe", "2").await;
        assert!(after[0].is_deleted());
        assert_eq!(after[0].delete_message.as_deref(), Some("probe replaced"));
        assert!(
            open_alarms(&state).await.is_empty(),
            "nothing stands, no alarm"
        );
        let response = send(
            &app,
            "GET",
            &format!("{}/{}.pdf", probe_path(LINE), row.id),
            &admin,
            None,
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(audit(&state, "device_calibration_delete").await.len(), 1);
    }

    /// Amendment 2026-10-14: the Hubs list flags a Hub whose probe needs
    /// calibration, and forgetting the Hub (removal, unpair) closes its
    /// probe reminders for good — the hourly sweep does not reopen them —
    /// until a probe upload on that Hub brings it back.
    #[tokio::test]
    async fn the_hubs_list_flags_a_probe_and_a_forgotten_hub_stays_closed() {
        let (state, _dir) = test_state().await;
        seed_node(&state);
        let manager = login_as_role(&state, Role::Manager, "manager1").await;
        let app = app(&state);

        let list = body_string(send(&app, "GET", "/hubs", &manager, None, Vec::new()).await).await;
        assert!(
            !list.contains("data-calibration"),
            "no icon before any upload: {list}"
        );
        upload(
            &app,
            &probe_path(LINE),
            &manager,
            upload_body(PDF, &date_in(-1)),
        )
        .await;
        let list = body_string(send(&app, "GET", "/hubs", &manager, None, Vec::new()).await)
            .await
            .replace("&#x2f;", "/");
        assert!(list.contains("Calibration: Expired"), "{list}");
        assert_eq!(open_alarms(&state).await.len(), 1);

        state.calibration.forget_hub("h1").await;
        assert!(open_alarms(&state).await.is_empty(), "closed with the Hub");
        state
            .calibration
            .check_all(time::OffsetDateTime::now_utc().unix_timestamp())
            .await;
        assert!(
            open_alarms(&state).await.is_empty(),
            "the sweep keeps it closed"
        );

        upload(
            &app,
            &probe_path(LINE),
            &manager,
            upload_body(PDF, &date_in(-1)),
        )
        .await;
        assert_eq!(
            open_alarms(&state).await.len(),
            1,
            "a new upload brings it back"
        );
    }
}
