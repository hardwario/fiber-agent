//! Add hub, Retry and Remove — the Server's half of a Client Hub
//! (plan/portal/devices.hub.md §4, §5, §8.1, §8.4; plan/control/cluster.md
//! §7–§9; milestone C5).
//!
//! ```text
//! POST /hubs/{server}/cluster/clients                    Add hub (hostname, gateway_eui, name)
//! POST /hubs/{server}/cluster/clients/{client}/retry     the same operation again: a new password
//! POST /hubs/{server}/cluster/clients/{client}/reveal    the one audited fetch of the join bundle
//! POST /hubs/{client}/cluster/remove                     cluster-client-remove (reason)
//! ```
//!
//! All four are admin (`RequireAdmin`, §8.1) and go through the signed relay to **the Server's**
//! agent — the one root process that can write the broker's files — never to the Client, which
//! is not on the broker before its join and has nothing to do with its own account.
//!
//! **The password** comes back once in `config/response.result` (C2), sealed to the reply key
//! this request signed (cluster.md §7, amended 2026-10-02), and is opened here in memory. It
//! leaves this module in one place: the one fetch of `…/reveal`. Until then the join bundle and
//! its QR code are held in memory only ([`crate::cluster::RevealVault`]: never on disk, never in
//! a page), under an unguessable token bound to the admin's session, the Server and the Client.
//! The completion screen carries the token, not the password. Its POST is the reveal, and it
//! writes the `hub_peer_secret_revealed` audit row itself (§8.5). It answers with the bundle
//! once; the entry is gone after that fetch, after 5 minutes, after a Retry and after a removal.
//! The password is not in the registry row, in any audit row, in a log line or in an SSE frame
//! (the relay keeps `result` off its status broadcast). Once it is gone, the way to a new one is
//! Retry, which rotates it, and the screen says so.
//!
//! **What is stored** (cluster.md §7): `pending` when the account is minted and the gateway is
//! registered or skipped, `failed` with the reason otherwise — "registration incomplete", never a
//! silent half. A partial answer that did mint an account shows **no** bundle: the Client would
//! join a half-registered cluster, and the Retry that finishes the registration rotates the
//! password anyway. [`crate::cluster::watch_liveness`] moves `pending` to `complete`.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Form, Router};
use minijinja::context;
use portal_storage::{ClusterInsert, NewClusterClient};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::{CurrentUser, RequireAdmin, SESSION_COOKIE};
use crate::cluster::{self, AddAnswer, ADD_VERB, REMOVE_VERB};
use crate::http::AppState;
use crate::relay::CommandOutcome;
use crate::render;

use super::command::Cmd;
use super::hub_routes::{hub_meta, is_local_device, known_hub};
use super::{not_found, render_fragment, tr};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/hubs/{hub}/cluster/clients", post(add_client))
        .route(
            "/hubs/{hub}/cluster/clients/{client}/retry",
            get(retry_form).post(retry_client),
        )
        .route(
            "/hubs/{hub}/cluster/clients/{client}/reveal",
            post(reveal_bundle),
        )
        .route("/hubs/{hub}/cluster/remove", post(remove_client))
        .route("/hubs/{hub}/cluster/gateway", post(grant_gateway))
}

/// The interim reason a row carries between its insert (or a Retry) and the agent's answer.
const NOT_FINISHED: &str = "registration did not finish";

/// §8.3: an operator's name for the Client, as for any Hub (§3.1).
const NAME_MAX: usize = 64;
/// §8.3: the removal reason.
const REASON_MAX: usize = 256;

// ---------------------------------------------------------------------------------------------
// The dialog's context
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Serialize)]
pub(crate) struct AddValues {
    pub hostname: String,
    pub gateway_eui: String,
    pub name: String,
}

#[derive(Debug, Default, Clone, Serialize)]
pub(crate) struct AddErrors {
    pub hostname: Option<String>,
    pub gateway_eui: Option<String>,
    pub name: Option<String>,
}

/// `partials/hub_add_form.html`'s `add_hub`: whether this Hub can take a Client at all, and why
/// not (§4 "Předpoklady vyhodnocené v dialogu" — the button never hides for these, §2.6).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AddHubCtx {
    pub server: String,
    /// `None` = the form is offered; otherwise the sentence that says why it is not.
    pub unavailable: Option<String>,
    pub values: AddValues,
    pub errors: AddErrors,
}

/// The preconditions, evaluated for the dialog (§4) — the same ones the POST checks again.
pub(crate) async fn add_hub_ctx(state: &AppState, server: &str) -> AddHubCtx {
    let mode = state.radio.mode().map(|m| m.as_str());
    let unavailable = if !is_local_device(server, &state.local_hostname) {
        Some(tr("monitoring.hubs.add_hub_not_server"))
    } else if matches!(mode, Some("off")) {
        Some(tr("monitoring.hubs.add_hub_radio_off"))
    } else if crate::routes::connection_routes::served_ca(state.ca_cert_path.to_path_buf())
        .await
        .is_none()
    {
        Some(tr("monitoring.hubs.add_hub_no_ca"))
    } else if matches!(state.keys.device_identity(server).await, Ok(None)) {
        Some(tr("monitoring.hubs.add_hub_unpaired"))
    } else {
        None
    };
    AddHubCtx {
        server: server.to_string(),
        unavailable,
        values: AddValues::default(),
        errors: AddErrors::default(),
    }
}

/// The Server this portal runs on, when it is in the store: the only Hub that can take a
/// Client (§4 precondition 1).
pub(crate) fn local_server(state: &AppState) -> Option<String> {
    let snapshot = state.store.snapshot();
    snapshot
        .devices
        .values()
        .find(|d| is_local_device(&d.hostname, &state.local_hostname))
        .map(|d| d.hostname.clone())
}

/// One Client row as the Hubs list and the Server's cluster card show it (§2.5, §3.4).
/// `device` is the Client's store entry, `None` until it has connected. `server_mode` is this
/// Server's radio mode, which every Client runs (decision #47).
pub(crate) fn client_row_ctx(
    row: &portal_storage::ClusterClient,
    device: Option<&crate::state::DeviceState>,
    server_mode: Option<&str>,
) -> render::ClusterRowCtx {
    render::ClusterRowCtx {
        hostname: row.hostname.clone(),
        gateway_eui: row.gateway_eui.clone(),
        peer_username: row.peer_username.clone(),
        state: match row.registration_state.as_str() {
            portal_storage::CLUSTER_STATE_PENDING => "pending",
            portal_storage::CLUSTER_STATE_COMPLETE => "complete",
            _ => "failed",
        },
        registration_error: row.registration_error.clone(),
        removal_error: row.removal_error.clone(),
        registered_at: row.registered_at.clone(),
        registered_by: row.registered_by.clone(),
        online: device.is_some_and(|d| d.online),
        never_connected: device.is_none(),
        gateway_gap: device
            .and_then(|d| cluster::gateway_gap(row.gateway_eui.as_deref(), d.lorawan.as_ref())),
        radio_mode: device
            .and_then(|d| d.lorawan.as_ref())
            .and_then(|l| l.mode.as_deref())
            .map(|m| m.trim().to_ascii_lowercase()),
        radio_mode_differs: crate::cluster_radio::differs(server_mode, device),
    }
}

// ---------------------------------------------------------------------------------------------
// POST /hubs/{server}/cluster/clients and …/{client}/retry
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct AddForm {
    #[serde(default)]
    hostname: String,
    #[serde(default)]
    gateway_eui: String,
    #[serde(default)]
    name: String,
}

/// What a reveal is bound to: the SHA-256 of this request's session cookie. `RequireAdmin` has
/// already validated the session, so the cookie is there.
fn session_of(headers: &HeaderMap) -> [u8; 32] {
    let jar = axum_extra::extract::cookie::CookieJar::from_headers(headers);
    cluster::session_fingerprint(
        jar.get(SESSION_COOKIE)
            .map(|c| c.value())
            .unwrap_or_default(),
    )
}

/// A response that holds a credential, or a token for one: no cache keeps it.
fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn add_client(
    RequireAdmin(user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    headers: HeaderMap,
    Form(form): Form<AddForm>,
) -> Response {
    let Some(server) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&user));
    };
    let mut ctx = add_hub_ctx(&state, &server).await;
    ctx.values = AddValues {
        hostname: form.hostname.trim().to_string(),
        gateway_eui: form.gateway_eui.trim().to_string(),
        name: form.name.trim().to_string(),
    };
    if ctx.unavailable.is_some() {
        return form_response(&state, &user, ctx);
    }

    // §8.3, every field on the server; the browser's pattern is UX only.
    let hostname = cluster::validate_hostname(&form.hostname);
    match &hostname {
        None => ctx.errors.hostname = Some(tr("monitoring.hubs.add_hub_err_hostname")),
        Some(h) if h.eq_ignore_ascii_case(&server) => {
            ctx.errors.hostname = Some(tr("monitoring.hubs.add_hub_err_hostname_server"))
        }
        Some(h) if state.cluster.get(h).is_some() => {
            ctx.errors.hostname = Some(tr("monitoring.hubs.add_hub_err_hostname_taken"))
        }
        Some(_) => {}
    }
    // The Add hub form asks for no EUI (amendment 2026-10-28): the Client reports it and the
    // admin grants it. A scripted POST may still send one.
    let eui = match check_eui(&state, &server, &form.gateway_eui, None) {
        Ok(eui) => eui,
        Err(why) => {
            ctx.errors.gateway_eui = Some(why);
            None
        }
    };
    let name = form.name.trim().to_string();
    if name.chars().count() > NAME_MAX || name.chars().any(char::is_control) {
        ctx.errors.name = Some(tr("monitoring.hubs.add_hub_err_name"));
    }
    let Some(hostname) = hostname else {
        return form_response(&state, &user, ctx);
    };
    if ctx.errors.hostname.is_some()
        || ctx.errors.gateway_eui.is_some()
        || ctx.errors.name.is_some()
    {
        return form_response(&state, &user, ctx);
    }

    // Claim the hostname and the EUI before anything is minted (§4 step 2, step 6).
    let claimed = state
        .cluster
        .insert(NewClusterClient {
            hostname: hostname.clone(),
            gateway_eui: eui.clone(),
            peer_username: cluster::peer_username(&hostname),
            registered_by: Some(user.username.clone()),
            registration_error: NOT_FINISHED.to_string(),
        })
        .await;
    match claimed {
        Ok(ClusterInsert::Inserted(_)) => {}
        Ok(ClusterInsert::HostnameTaken) => {
            ctx.errors.hostname = Some(tr("monitoring.hubs.add_hub_err_hostname_taken"));
            return form_response(&state, &user, ctx);
        }
        Ok(ClusterInsert::GatewayEuiTaken) => {
            ctx.errors.gateway_eui = Some(tr("monitoring.hubs.add_hub_err_eui_taken"));
            return form_response(&state, &user, ctx);
        }
        Err(err) => {
            tracing::error!(%err, hostname, "could not write the cluster_clients row");
            return Cmd::new(&state, &user, ADD_VERB)
                .fail(&tr("monitoring.hubs.add_hub_err_store"));
        }
    }
    if !name.is_empty() {
        let saved = super::edit_device_meta(
            &state,
            &user,
            &hostname,
            portal_storage::THRESHOLD_SCOPE_HUB,
            &hostname,
            |row| row.name = Some(name.clone()),
        )
        .await;
        if let Err(err) = saved {
            // The name is a convenience; the registration does not hang on it.
            tracing::warn!(%err, hostname, "could not save the new Client's name");
        }
    }

    register(&state, &user, &headers, &server, &hostname, eui, None).await
}

/// The gateway EUI of Add hub and of a Retry that changes it (§8.3, #163): empty is `None` unless
/// the Server runs LoRaWAN; otherwise 16 hex digits, not the Server's own EUI and not another
/// Client's. `own` is the Client a Retry is for, which may keep its EUI.
fn check_eui(
    state: &AppState,
    server: &str,
    raw: &str,
    own: Option<&str>,
) -> Result<Option<String>, String> {
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let Some(eui) = cluster::canonical_eui(raw) else {
        return Err(tr("monitoring.hubs.add_hub_err_eui"));
    };
    let server_eui = state
        .store
        .snapshot()
        .device(server)
        .and_then(|d| d.lorawan.as_ref())
        .and_then(|l| l.gateway_eui.clone());
    if server_eui.as_deref() == Some(eui.as_str()) {
        return Err(tr("monitoring.hubs.add_hub_err_eui_server"));
    }
    match state.cluster.eui_owner(&eui) {
        Some(owner) if Some(owner.as_str()) != own => {
            Err(tr("monitoring.hubs.add_hub_err_eui_taken"))
        }
        _ => Ok(Some(eui)),
    }
}

// ---------------------------------------------------------------------------------------------
// GET|POST /hubs/{server}/cluster/clients/{client}/retry
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct RetryCtx {
    server: String,
    client: String,
}

/// The Retry / new-password confirmation (#173: no gateway EUI; the Client's gateway comes from
/// its own report and the admin's grant, #164). An unavailable Add hub answers with its reason,
/// as the form does.
async fn retry_form(
    RequireAdmin(user): RequireAdmin,
    State(state): State<AppState>,
    Path((hub, client)): Path<(String, String)>,
) -> Response {
    let Some(server) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&user));
    };
    let Some(row) = state.cluster.get(&client) else {
        return not_found(&state, Some(&user));
    };
    let ctx = add_hub_ctx(&state, &server).await;
    if ctx.unavailable.is_some() {
        return form_response(&state, &user, ctx);
    }
    let retry = RetryCtx {
        server,
        client: row.hostname.clone(),
    };
    let ctx = render::page_ctx(context! { retry => retry }, Some(&user));
    render_fragment(&state, "partials/hub_retry_form.html", ctx)
}

/// Rotates the password and keeps the registered gateway; a submitted `gateway_eui` is ignored
/// (#173).
async fn retry_client(
    RequireAdmin(user): RequireAdmin,
    State(state): State<AppState>,
    Path((hub, client)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(server) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&user));
    };
    let Some(row) = state.cluster.get(&client) else {
        return not_found(&state, Some(&user));
    };
    let ctx = add_hub_ctx(&state, &server).await;
    if let Some(why) = ctx.unavailable.as_deref() {
        return Cmd::new(&state, &user, ADD_VERB).fail(why);
    }
    if let Err(err) = state
        .cluster
        .begin_attempt(&row.hostname, NOT_FINISHED, None)
        .await
    {
        tracing::error!(%err, hostname = %row.hostname, "could not mark the retry");
        return Cmd::new(&state, &user, ADD_VERB).fail(&tr("monitoring.hubs.add_hub_err_store"));
    }
    let hostname = row.hostname.clone();
    let eui = row.gateway_eui.clone();
    register(&state, &user, &headers, &server, &hostname, eui, Some(row)).await
}

/// `true` when the request never left the portal: refused before signing, or signed but not
/// published because the broker could not be reached. The agent minted and rotated nothing.
fn never_sent(outcome: &CommandOutcome) -> bool {
    outcome.request_id.is_none()
        || outcome.error_code == Some(crate::relay::error_code::MQTT_UNAVAILABLE)
}

/// The one operation behind Add hub and Retry: relay `cluster-client-add` to the Server, store
/// what it answered, audit it, and show the bundle or say what is incomplete.
///
/// `previous` is the row as a Retry found it, before [`crate::cluster::Registry::begin_attempt`]
/// marked it `failed`; `None` for Add hub. A Retry that never reaches the agent puts it back,
/// because the account it would have rotated still works.
async fn register(
    state: &AppState,
    user: &CurrentUser,
    headers: &HeaderMap,
    server: &str,
    client: &str,
    eui: Option<String>,
    previous: Option<portal_storage::ClusterClient>,
) -> Response {
    let retry = previous.is_some();
    let cmd = Cmd::new(state, user, ADD_VERB);
    // This run rotates the password, so whatever bundle is still waiting for a reveal is dead.
    state.reveals.forget_client(client);
    let audit_event = if retry {
        "hub_client_registration_retry"
    } else {
        "hub_client_registered"
    };

    // The fingerprint is taken now, from the bytes the CA route serves: a bundle is never
    // issued against a CA that is not there (cluster.md §6).
    let Some(ca) =
        crate::routes::connection_routes::served_ca(state.ca_cert_path.to_path_buf()).await
    else {
        if let Some(previous) = &previous {
            let _ = state.cluster.restore(previous).await;
        } else {
            let _ = state
                .cluster
                .record_result(
                    client,
                    &cluster::peer_username(client),
                    Some("no CA certificate"),
                )
                .await;
        }
        return cmd.fail(&tr("monitoring.hubs.add_hub_no_ca"));
    };
    let fingerprint = cluster::ca_fingerprint(ca.as_bytes());

    // The agent seals the new password to this request's own key (cluster.md §7, amended
    // 2026-10-02): its answer travels on the loopback bus, which anyone local can read. The
    // public half is a signed parameter; the secret never leaves this function except into
    // `AddAnswer::take`, which consumes it.
    let reply = fiber_protocol::sealed_reply::ReplySecret::generate();
    let mut params = json!({ "hostname": client, "reply_key": reply.reply_key() });
    if let Some(e) = &eui {
        params["gateway_eui"] = json!(e);
    }

    let mut outcome = match cmd.execute(server, params, None).await {
        Ok(outcome) if !never_sent(&outcome) => outcome,
        Ok(refused) | Err(refused) => {
            // Nothing reached the agent, so nothing was minted or rotated: a first attempt
            // leaves no row behind, a Retry puts its row back as it was.
            if let Some(previous) = &previous {
                if let Err(err) = state.cluster.restore(previous).await {
                    tracing::error!(%err, hostname = client, "could not restore the row");
                }
            } else {
                let _ = state.cluster.delete(client).await;
                super::forget_device_meta(
                    state,
                    client,
                    portal_storage::THRESHOLD_SCOPE_HUB,
                    client,
                )
                .await;
            }
            return cmd.done(refused);
        }
    };

    let answer = outcome
        .result
        .take()
        .map(|r| AddAnswer::take(r.0, Some(reply), outcome.request_id.as_deref(), client));
    let username = answer
        .as_ref()
        .and_then(|a| a.username.clone())
        .unwrap_or_else(|| cluster::peer_username(client));
    let complete = answer.as_ref().is_some_and(AddAnswer::complete);
    let reason = (!complete).then(|| incomplete_reason(&outcome, answer.as_ref()));
    let stored = state
        .cluster
        .record_result(client, &username, reason.as_deref())
        .await;
    if let Err(err) = &stored {
        tracing::error!(%err, hostname = client, "could not store the registration's outcome");
    }
    // A Remove that ran while this command was out deleted the row: the Client is no longer
    // registered here, so no bundle is issued for it.
    let removed = matches!(stored, Ok(None));

    let (account, gateway) = answer
        .as_ref()
        .map(|a| (a.account, a.gateway))
        .unwrap_or(("unknown", "unknown"));
    // What the agent says it registered, else what was asked for.
    let audited_eui = answer
        .as_ref()
        .and_then(|a| a.gateway_eui.clone())
        .or_else(|| eui.clone());
    let registration_state = if removed {
        "removed"
    } else if complete {
        "pending"
    } else {
        "failed"
    };
    // The audit row's `success` is the outcome's `status`, which C2 makes SUCCESS exactly when
    // the account is ok and the gateway not failed. No password — `answer` has none in it.
    cmd.audit(
        audit_event,
        client,
        json!({
            "command_type": ADD_VERB,
            "request_id": outcome.request_id,
            "server": server,
            "peer_username": username,
            "gateway_eui": audited_eui,
            "account": account,
            "gateway": gateway,
            "registration_state": registration_state,
            "error": reason,
        }),
        &outcome,
    )
    .await;

    if removed {
        // `answer` and its password are dropped, and wiped, here.
        drop(answer);
        return cmd.fail(&tr("monitoring.hubs.add_hub_err_removed"));
    }
    let password = answer.and_then(|a| a.password);
    match (complete, password) {
        (true, Some(password)) => {
            // The bundle goes into memory for one fetch; the page gets the token for it.
            let bundle = cluster::join_bundle(server, &password, &fingerprint);
            drop(password);
            let qr = cluster::qr_svg(&bundle);
            let token = state
                .reveals
                .put(session_of(headers), user.id, server, client, bundle, qr);
            let ctx = render::page_ctx(
                context! {
                    server => server,
                    client => client,
                    host => format!("{}.local", server.to_ascii_lowercase()),
                    port => cluster::BROKER_PORT,
                    peer_username => username,
                    fingerprint => fingerprint,
                    gateway => gateway,
                    token => token,
                    rotated => retry,
                },
                Some(user),
            );
            no_store(render_fragment(state, "partials/hub_join_bundle.html", ctx))
        }
        _ => {
            let ctx = render::page_ctx(
                context! {
                    server => server,
                    client => client,
                    account => account,
                    gateway => gateway,
                    reason => reason,
                    // With the agent's result in hand the effects say what
                    // happened; "the device rejected the command" would not.
                    error_code => if account == "unknown" { outcome.error_code } else { None },
                    status => outcome.status,
                },
                Some(user),
            );
            render_fragment(state, "partials/hub_add_incomplete.html", ctx)
        }
    }
}

/// Why a registration is incomplete, in the words the operator can act on, kept with the row.
fn incomplete_reason(outcome: &CommandOutcome, answer: Option<&AddAnswer>) -> String {
    if outcome.status == "timeout" {
        return "no answer from the Server's agent".to_string();
    }
    match answer {
        Some(a) if a.seal_problem == Some(cluster::SealProblem::InClear) => {
            "the Server's agent sent the password in clear, so it is not used; update the \
             Server and Retry"
                .to_string()
        }
        Some(a) if a.seal_problem == Some(cluster::SealProblem::DoesNotOpen) => {
            "the password the Server's agent sealed does not open with this request's key; \
             Retry issues a new one"
                .to_string()
        }
        Some(a) => a
            .message
            .clone()
            .unwrap_or_else(|| format!("account {}, gateway {}", a.account, a.gateway)),
        None => outcome
            .message
            .clone()
            .unwrap_or_else(|| "the Server's agent answered without a result".to_string()),
    }
}

fn form_response(state: &AppState, user: &CurrentUser, add_hub: AddHubCtx) -> Response {
    let ctx = render::page_ctx(context! { add_hub => add_hub }, Some(user));
    render_fragment(state, "partials/hub_add_form.html", ctx)
}

// ---------------------------------------------------------------------------------------------
// POST /hubs/{server}/cluster/clients/{client}/reveal
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct RevealForm {
    #[serde(default)]
    token: String,
}

/// The reveal of §4 step 4 / §8.4: the one fetch of the join bundle, from memory, by the admin
/// and session it was issued to. A POST, so the portal's origin guard covers it like every other
/// state change, and the token stays out of URLs and server logs. This handler writes the
/// audit row (`hub_peer_secret_revealed`, §8.5) itself, without the secret. A refused fetch is
/// audited as well, as a failure, with why. The bundle leaves memory with this answer whatever
/// it says.
async fn reveal_bundle(
    RequireAdmin(user): RequireAdmin,
    State(state): State<AppState>,
    Path((hub, client)): Path<(String, String)>,
    headers: HeaderMap,
    Form(form): Form<RevealForm>,
) -> Response {
    let Some(server) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&user));
    };
    let Some(row) = state.cluster.get(&client) else {
        return not_found(&state, Some(&user));
    };
    let fetched = state.reveals.take(
        form.token.trim(),
        session_of(&headers),
        user.id,
        &server,
        &row.hostname,
    );
    let details = |refused: Option<&str>| {
        json!({
            "server": server,
            "peer_username": row.peer_username,
            "refused": refused,
        })
    };
    match fetched {
        Ok(revealed) => {
            super::audit_action(
                &state,
                &user,
                "hub_peer_secret_revealed",
                &row.hostname,
                details(None),
                true,
            )
            .await;
            let ctx = render::page_ctx(
                context! {
                    bundle => revealed.bundle.as_str(),
                    qr_svg => revealed.qr_svg.as_ref().map(|q| q.as_str()),
                },
                Some(&user),
            );
            no_store(render_fragment(
                &state,
                "partials/hub_join_bundle_revealed.html",
                ctx,
            ))
        }
        Err(refusal) => {
            super::audit_action(
                &state,
                &user,
                "hub_peer_secret_revealed",
                &row.hostname,
                details(Some(refusal.as_str())),
                false,
            )
            .await;
            let ctx = render::page_ctx(
                context! {
                    server => server,
                    client => row.hostname,
                    other_session => refusal == cluster::RevealRefusal::OtherSession,
                },
                Some(&user),
            );
            no_store(render_fragment(
                &state,
                "partials/hub_join_bundle_gone.html",
                ctx,
            ))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// POST /hubs/{client}/cluster/gateway
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct GatewayForm {
    #[serde(default)]
    gateway_eui: String,
}

/// "Povolit bránu" (cluster.md amendment 2026-10-28, #164): the admin grants a registered Client
/// the gateway EUI it reports. The signed `cluster-client-gateway` writes the ACL line and
/// registers the gateway on the Server without rotating the password; on success the registry
/// row takes the EUI. The Server never does this on the Client's report alone.
async fn grant_gateway(
    RequireAdmin(user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<GatewayForm>,
) -> Response {
    let Some(client) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&user));
    };
    let cmd = Cmd::new(&state, &user, cluster::GATEWAY_VERB);
    let Some(row) = state.cluster.get(&client) else {
        return cmd.fail(&tr("monitoring.hubs.remove_not_client"));
    };
    let Some(eui) = cluster::canonical_eui(&form.gateway_eui) else {
        return cmd.fail(&tr("monitoring.hubs.add_hub_err_eui"));
    };
    if state
        .cluster
        .eui_owner(&eui)
        .is_some_and(|owner| owner != row.hostname)
    {
        return cmd.fail(&tr("monitoring.hubs.add_hub_err_eui_taken"));
    }
    let Some(server) = local_server(&state) else {
        return cmd.fail(&tr("monitoring.hubs.add_hub_not_server"));
    };
    let outcome = match cmd
        .execute(
            &server,
            json!({ "hostname": row.hostname, "gateway_eui": eui }),
            None,
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(refused) => return cmd.done(refused),
    };
    let granted = super::command::succeeded(&outcome);
    if granted {
        match state.cluster.set_gateway_eui(&row.hostname, &eui).await {
            Ok(true) => {}
            Ok(false) => tracing::warn!(hostname = %row.hostname, %eui, "the EUI is another row's"),
            Err(err) => {
                tracing::error!(%err, hostname = %row.hostname, "could not store the granted EUI")
            }
        }
    }
    cmd.audit(
        "hub_client_gateway_granted",
        &row.hostname,
        json!({
            "command_type": cluster::GATEWAY_VERB,
            "request_id": outcome.request_id,
            "server": server,
            "gateway_eui": eui,
            "previous_gateway_eui": row.gateway_eui,
        }),
        &outcome,
    )
    .await;
    if granted {
        cmd.done(CommandOutcome {
            message: Some(tr("monitoring.hubs.gateway_granted").replace("{eui}", &eui)),
            ..outcome
        })
    } else {
        cmd.done(outcome)
    }
}

// ---------------------------------------------------------------------------------------------
// POST /hubs/{client}/cluster/remove
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct RemoveForm {
    #[serde(default)]
    reason: String,
    /// The Client's hostname typed by the operator (#170); checked here, the browser's
    /// `pattern` is only a hint.
    #[serde(default)]
    confirm_hostname: String,
    /// Ticked when the operator accepts removing an unpaired Client without its factory reset:
    /// nothing can sign the reset, so the Hub keeps its Client role until it is reset on site.
    #[serde(default)]
    accept_no_reset: Option<String>,
}

/// The Client's factory reset ahead of its removal (cluster.md amendment 2026-10-29, #171):
/// `done`, `not_delivered` or `skipped` (not paired, nothing can sign for it).
async fn reset_client(
    state: &AppState,
    user: &CurrentUser,
    hostname: &str,
    reason: &str,
) -> (&'static str, Option<String>) {
    if !state.keys.is_paired(hostname).await {
        return ("skipped", None);
    }
    let cmd = Cmd::new(state, user, "factory_reset");
    match cmd
        .execute(
            hostname,
            json!({ "post_action": "reboot" }),
            Some(reason.to_string()),
        )
        .await
    {
        Ok(outcome) => {
            cmd.audit(
                "device_factory_reset",
                hostname,
                json!({ "command": "factory_reset", "via": REMOVE_VERB }),
                &outcome,
            )
            .await;
            let result = if super::command::succeeded(&outcome) {
                "done"
            } else {
                "not_delivered"
            };
            (result, outcome.request_id)
        }
        Err(_) => ("not_delivered", None),
    }
}

/// "Odebrat z clusteru" (§5): the typed hostname (#170), the Client's factory reset while its
/// keys still exist (#171), then `cluster-client-remove` on the Server — the account and its
/// sessions end, the gateway is deregistered — then, only when both are done, this portal's
/// pairing keys for the Client and its registry row. Otherwise the row stays with "removal
/// incomplete" and the same button retries it.
async fn remove_client(
    RequireAdmin(user): RequireAdmin,
    State(state): State<AppState>,
    Path(hub): Path<String>,
    Form(form): Form<RemoveForm>,
) -> Response {
    let Some(client) = known_hub(&state, &hub) else {
        return not_found(&state, Some(&user));
    };
    let cmd = Cmd::new(&state, &user, REMOVE_VERB);
    let Some(row) = state.cluster.get(&client) else {
        // A Hub that is not a registered Client — the Server itself above all (§5: "Tento Hub
        // se nikdy neodebírá") — has nothing to remove.
        return cmd.fail(&tr("monitoring.hubs.remove_not_client"));
    };
    let reason = form.reason.trim().to_string();
    if reason.is_empty() || reason.chars().count() > REASON_MAX {
        return cmd.fail(&tr("monitoring.hubs.remove_reason_required"));
    }
    if form.confirm_hostname.trim() != row.hostname {
        return cmd.fail(&tr("monitoring.hubs.remove_hostname_mismatch"));
    }
    if form.accept_no_reset.is_none() && !state.keys.is_paired(&row.hostname).await {
        return cmd.fail(&tr("monitoring.hubs.remove_unpaired_not_accepted"));
    }
    let Some(server) = local_server(&state) else {
        return cmd.fail(&tr("monitoring.hubs.add_hub_not_server"));
    };
    // A removal ends the account, so a bundle still waiting for its reveal is dead.
    state.reveals.forget_client(&row.hostname);
    // Before the Server's half and the unpair: afterwards nothing could sign for the Client.
    // A reset that does not arrive does not stop the removal.
    let (reset, reset_request_id) = reset_client(&state, &user, &row.hostname, &reason).await;

    let mut outcome = match cmd
        .execute(
            &server,
            json!({ "hostname": row.hostname }),
            Some(reason.clone()),
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(refused) => return cmd.done(refused),
    };
    let answer = outcome
        .result
        .take()
        .map(|r| AddAnswer::take(r.0, None, None, &row.hostname));
    let (account, gateway) = answer
        .as_ref()
        .map(|a| (a.account, a.gateway))
        .unwrap_or(("unknown", "unknown"));
    let complete = account == "ok" && gateway != "failed" && answer.is_some();

    let error = (!complete).then(|| incomplete_reason(&outcome, answer.as_ref()));
    if complete {
        // §5: the portal's part runs only after the Server's half succeeded.
        match state.keys.unpair(&row.hostname).await {
            Ok(()) | Err(crate::keys::KeyError::NotPaired(_)) => {}
            Err(err) => {
                tracing::warn!(%err, hostname = %row.hostname, "could not drop the Client's keys")
            }
        }
        super::forget_device_meta(
            &state,
            &row.hostname,
            portal_storage::THRESHOLD_SCOPE_HUB,
            &row.hostname,
        )
        .await;
        // calibration.cs.md amendment 2026-10-14: its probe reminders close.
        state.calibration.forget_hub(&row.hostname).await;
        // cluster.md amendment 2026-10-28 (#168): the Hub leaves the list now; the agent has
        // cleared its retained documents, so a restart does not bring it back.
        state.store.forget_device(&row.hostname);
        if let Err(err) = state.cluster.delete(&row.hostname).await {
            tracing::error!(%err, hostname = %row.hostname, "could not delete the registry row");
        }
    } else if let Err(err) = state
        .cluster
        .record_removal_error(&row.hostname, error.as_deref().unwrap_or_default())
        .await
    {
        tracing::error!(%err, hostname = %row.hostname, "could not record the failed removal");
    }

    cmd.audit(
        "hub_client_removed",
        &row.hostname,
        json!({
            "command_type": REMOVE_VERB,
            "request_id": outcome.request_id,
            "server": server,
            "peer_username": row.peer_username,
            "gateway_eui": row.gateway_eui,
            "account": account,
            "gateway": gateway,
            "reason": reason,
            "error": error,
            "factory_reset": reset,
            "factory_reset_request_id": reset_request_id,
        }),
        &outcome,
    )
    .await;

    // Report each effect (§5), in the operator's language.
    let effects = tr("monitoring.hubs.remove_effects")
        .replace(
            "{account}",
            &tr(&format!("monitoring.hubs.effect_{account}")),
        )
        .replace(
            "{gateway}",
            &tr(&format!("monitoring.hubs.effect_{gateway}")),
        )
        .replace(
            "{reset}",
            &tr(&format!("monitoring.hubs.effect_reset_{reset}")),
        );
    if complete {
        // The Client's pages are gone with its row; the message says so and the next
        // navigation shows the list without it.
        cmd.done(CommandOutcome {
            status: "success",
            message: Some(format!("{} {effects}", tr("monitoring.hubs.removed"))),
            error_code: None,
            ..outcome
        })
    } else {
        cmd.done(CommandOutcome {
            status: "error",
            // As for Add hub: with the effects reported, the generic
            // "device rejected" sentence would say less than the message.
            error_code: if answer.is_some() {
                None
            } else {
                outcome.error_code
            },
            message: Some(format!(
                "{} {effects}{}",
                tr("monitoring.hubs.remove_incomplete"),
                error.map(|e| format!(" ({e})")).unwrap_or_default()
            )),
            ..outcome
        })
    }
}

/// The client's own name for the cluster card and the removal dialog.
pub(crate) async fn client_name(state: &AppState, hostname: &str) -> Option<String> {
    hub_meta(state, hostname).await.and_then(|m| m.name)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use fiber_protocol::payloads::config_challenge::ConfigChallengeMsg;
    use fiber_protocol::payloads::config_response::{ConfigResponseMsg, ResponseResult};
    use fiber_protocol::payloads::status::DeviceStatus;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use crate::auth::{Role, SESSION_COOKIE};
    use crate::http::AppState;
    use crate::routes::test_support::{login, login_as_role, test_state};
    use crate::state::IngestEvent;

    const SERVER: &str = "proximos-a1b2c3d4";
    const CLIENT: &str = "proximos-1a2b3c4d";
    const EUI: &str = "70b3d54900aa0002";
    /// 22 characters of base64url, as the agent mints them.
    const PASSWORD: &str = "Zq3-Xr9_Lm2Kp7Wn4Vb8Tc";
    const CA_PEM: &str =
        "-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIQc5\n-----END CERTIFICATE-----\n";

    struct App {
        router: axum::Router,
        state: AppState,
        _dir: tempfile::TempDir,
        _radio: tempfile::TempDir,
        admin: String,
        relay_tx: tokio::sync::mpsc::UnboundedSender<IngestEvent>,
    }

    /// The Server `SERVER` as the Hub this portal runs on: online, paired,
    /// on LoRaWAN, with its CA in place — every precondition of §4 met.
    async fn app() -> App {
        app_with_ttl(crate::cluster::REVEAL_TTL).await
    }

    /// [`app`] with the reveal vault holding a bundle for `ttl`.
    async fn app_with_ttl(ttl: std::time::Duration) -> App {
        app_with(ttl, None).await
    }

    /// [`app_with_ttl`] with `relay` in place of the test relay.
    async fn app_with(ttl: std::time::Duration, relay: Option<crate::relay::CommandRelay>) -> App {
        let (mut state, dir) = test_state().await;
        if let Some(relay) = relay {
            state.relay = relay;
        }
        state.reveals = crate::cluster::RevealVault::new(ttl);
        state.local_hostname = SERVER.to_string();
        let radio = tempfile::tempdir().unwrap();
        state.radio =
            crate::radio::RadioControl::for_tests(control_radio::Mode::Lorawan, radio.path());
        std::fs::create_dir_all(state.ca_cert_path.parent().unwrap()).unwrap();
        std::fs::write(state.ca_cert_path.as_ref(), CA_PEM).unwrap();
        state.store.apply(IngestEvent::Status {
            hostname: SERVER.to_string(),
            status: DeviceStatus::Online,
        });
        state
            .keys
            .store_pairing(
                SERVER,
                Some("test-ca".into()),
                "00".repeat(32),
                [7u8; 32],
                None,
                None,
            )
            .await
            .expect("pair the Server");
        let (relay_tx, relay_rx) = tokio::sync::mpsc::unbounded_channel();
        let run_relay = state.relay.clone();
        tokio::spawn(async move { run_relay.run(relay_rx).await });
        let admin = login(&state, "admin", "correct-horse-battery").await;
        let router = crate::routes::hub_routes::router()
            .merge(super::router())
            .merge(crate::routes::connection_routes::router())
            .with_state(state.clone());
        App {
            router,
            state,
            _dir: dir,
            _radio: radio,
            admin,
            relay_tx,
        }
    }

    async fn send(
        app: &App,
        method: &str,
        uri: &str,
        token: &str,
        body: String,
    ) -> axum::response::Response {
        app.router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn text(response: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    /// The one-time reveal token in a completion screen.
    fn token_in(html: &str) -> String {
        let marker = "name=\"token\" value=\"";
        let at = html.find(marker).expect("a reveal token") + marker.len();
        html[at..].split('"').next().unwrap().to_string()
    }

    /// `POST …/reveal` with `token`, as the admin session `session`.
    async fn reveal(app: &App, session: &str, token: &str) -> axum::response::Response {
        send(
            app,
            "POST",
            &format!("/hubs/{SERVER}/cluster/clients/{CLIENT}/reveal"),
            session,
            format!("token={token}"),
        )
        .await
    }

    /// A request's params without the per-request `reply_key`.
    fn without_reply_key(params: &Value) -> Value {
        let mut params = params.clone();
        params.as_object_mut().unwrap().remove("reply_key");
        params
    }

    fn requests(state: &AppState) -> Vec<(String, Value)> {
        state
            .relay
            .published()
            .into_iter()
            .filter(|(topic, _)| topic.ends_with("commands/config/request"))
            .collect()
    }

    /// Plays the Server's agent for the `n`-th signed request: challenge,
    /// wait for the confirm, then answer with `status` and `result`. A
    /// `password` in `result` is sealed to the request's `reply_key`, as C2's
    /// agent does; [`answer_in_clear`] sends it as it is.
    async fn answer(app: &App, n: usize, status: &str, result: Option<Value>) {
        answer_as(app, n, status, result, true).await
    }

    /// An agent from before the seal: the password in clear.
    async fn answer_in_clear(app: &App, n: usize, status: &str, result: Option<Value>) {
        answer_as(app, n, status, result, false).await
    }

    async fn answer_as(app: &App, n: usize, status: &str, result: Option<Value>, seal: bool) {
        let nth = |suffix: &'static str| {
            let state = app.state.clone();
            async move {
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
            }
        };
        let request = nth("commands/config/request").await;
        let request_id = request["request_id"].as_str().unwrap().to_string();
        let result = result.map(|mut doc| {
            if let (true, Some(Value::String(pw))) = (seal, doc.get("password").cloned()) {
                let sealed = fiber_protocol::sealed_reply::seal(
                    &pw,
                    request["params"]["reply_key"]
                        .as_str()
                        .expect("cluster-client-add signs a reply_key"),
                    &request_id,
                    doc["hostname"].as_str().unwrap(),
                )
                .unwrap();
                doc.as_object_mut().unwrap().remove("password");
                doc["password_sealed"] = serde_json::to_value(sealed).unwrap();
            }
            doc
        });
        let challenge_id = format!("chal-{n}");
        app.relay_tx
            .send(IngestEvent::ConfigChallenge {
                hostname: SERVER.into(),
                payload: ConfigChallengeMsg {
                    challenge_id: challenge_id.clone(),
                    request_id: Some(request_id),
                    signer_id: None,
                    expires_at: None,
                    preview: None,
                },
            })
            .unwrap();
        nth("commands/config/confirm").await;
        app.relay_tx
            .send(IngestEvent::ConfigResponse {
                hostname: SERVER.into(),
                payload: ConfigResponseMsg {
                    challenge_id,
                    request_id: None,
                    status: status.into(),
                    applied_at: None,
                    effective_at: None,
                    message: Some(format!("{status} from the mock agent")),
                    result: result.map(ResponseResult),
                },
            })
            .unwrap();
    }

    fn add_result(gateway: &str) -> Value {
        json!({
            "hostname": CLIENT,
            "username": format!("peer-{CLIENT}"),
            "gateway_eui": EUI,
            "account": "ok",
            "gateway": gateway,
            "password": PASSWORD,
            "message": format!("account peer-{CLIENT} minted; gateway {gateway}"),
        })
    }

    /// Runs `POST uri` while the mock agent answers it.
    /// A removal of a paired Client signs two requests (#171): the Client's factory reset
    /// (`n`), answered with `reset`, then `cluster-client-remove` to the Server (`n + 1`).
    async fn remove_answered(
        app: &App,
        body: String,
        n: usize,
        reset: &str,
        status: &str,
        result: Option<Value>,
    ) -> axum::response::Response {
        let uri = format!("/hubs/{CLIENT}/cluster/remove");
        let request = send(app, "POST", &uri, &app.admin, body);
        let answers = async {
            answer(app, n, reset, None).await;
            answer(app, n + 1, status, result).await;
        };
        let (response, ()) = tokio::join!(request, answers);
        response
    }

    async fn post_answered(
        app: &App,
        uri: &str,
        body: String,
        n: usize,
        status: &str,
        result: Option<Value>,
    ) -> axum::response::Response {
        let request = send(app, "POST", uri, &app.admin, body);
        let (response, ()) = tokio::join!(request, answer(app, n, status, result));
        response
    }

    async fn audit(state: &AppState, event: &str) -> Vec<portal_storage::AuditEntry> {
        state
            .auth
            .audit_page(portal_storage::AuditFilter {
                event_type: Some(event.to_string()),
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap()
            .0
    }

    /// Every byte the portal wrote under its data directory: the databases,
    /// their WAL files, the spill journal.
    fn every_byte_on_disk(state: &AppState) -> Vec<u8> {
        fn walk(dir: &std::path::Path, out: &mut Vec<u8>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    out.extend(bytes);
                }
            }
        }
        let mut out = Vec::new();
        walk(std::path::Path::new(&state.data_dir), &mut out);
        out
    }

    /// A catalogue sentence as the templates' HTML escaping renders it.
    fn t(key: &str) -> String {
        crate::routes::tr(key)
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#x27;")
            .replace('/', "&#x2f;")
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
    }

    /// Captures every log line the current thread emits, at every level.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// §4 + §8.4 end to end: the bundle reaches the admin once, as QR and as
    /// text, and the password is in no row, no audit entry, no log line and
    /// no status event.
    #[tokio::test]
    async fn add_hub_shows_the_bundle_once_and_keeps_the_password_nowhere() {
        let logs = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(logs.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::info!("capture-probe");

        let app = app().await;
        let mut status_rx = app.state.relay.subscribe_status();
        let response = post_answered(
            &app,
            &format!("/hubs/{SERVER}/cluster/clients"),
            "hostname=PROXIMOS-1A2B3C4D&gateway_eui=70%3AB3%3AD5%3A49%3A00%3AAA%3A00%3A02&name=Sklad+B".to_string(),
            0,
            "SUCCESS",
            Some(add_result("ok")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = text(response).await;

        // What was signed: the canonical hostname and EUI, to the Server.
        let sent = requests(&app.state);
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].0,
            "proximos/proximos-a1b2c3d4/commands/config/request"
        );
        assert_eq!(sent[0].1["command_type"], "cluster-client-add");
        assert_eq!(
            without_reply_key(&sent[0].1["params"]),
            json!({"hostname": CLIENT, "gateway_eui": EUI})
        );
        // And the request's own X25519 public key, which the password comes back sealed to.
        assert_eq!(sent[0].1["params"]["reply_key"].as_str().unwrap().len(), 43);

        // The completion screen: the token for the reveal, and nothing secret.
        let fingerprint = crate::cluster::ca_fingerprint(CA_PEM.as_bytes());
        let bundle =
            format!(r#"{{"v":1,"h":"{SERVER}.local","p":"{PASSWORD}","f":"{fingerprint}"}}"#);
        let escaped = bundle.replace('"', "&quot;");
        assert!(
            !html.contains(PASSWORD),
            "no password before the reveal: {html}"
        );
        assert!(!html.contains(&escaped), "no bundle before the reveal");
        let qr = crate::cluster::qr_svg(&bundle).unwrap();
        assert!(!html.contains(qr.as_str()), "no QR code before the reveal");
        assert!(html.contains(&format!("/hubs/{SERVER}/cluster/clients/{CLIENT}/reveal")));
        let token = token_in(&html);
        assert_eq!(token.len(), 43);
        assert!(
            !contains(&every_byte_on_disk(&app.state), PASSWORD),
            "held in memory, not on disk"
        );
        assert!(
            audit(&app.state, "hub_peer_secret_revealed")
                .await
                .is_empty(),
            "nothing is revealed yet"
        );

        // The reveal: the bundle as text and as an inline QR code, once, not cached.
        let revealed = reveal(&app, &app.admin, &token).await;
        assert_eq!(revealed.status(), StatusCode::OK);
        assert_eq!(
            revealed.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let shown = text(revealed).await;
        assert!(shown.contains(&escaped), "the bundle text: {shown}");
        assert!(shown.contains(qr.as_str()), "and its QR code");
        let reveals = audit(&app.state, "hub_peer_secret_revealed").await;
        assert_eq!(reveals.len(), 1, "the fetch wrote its own audit row");
        assert!(reveals[0].success);
        assert_eq!(reveals[0].target.as_deref(), Some(CLIENT));
        assert_eq!(
            app.state.reveals.held(),
            0,
            "gone from memory with the fetch"
        );

        // The second fetch reveals nothing and says Retry rotates.
        let again = text(reveal(&app, &app.admin, &token).await).await;
        assert!(!again.contains(PASSWORD), "{again}");
        assert!(again.contains(&t("monitoring.hubs.bundle_gone_title")));
        assert!(again.contains(&t("monitoring.hubs.bundle_gone_retry")));
        assert!(again.contains(&format!("/hubs/{SERVER}/cluster/clients/{CLIENT}/retry")));
        let reveals = audit(&app.state, "hub_peer_secret_revealed").await;
        assert_eq!(reveals.len(), 2);
        assert_eq!(
            reveals.iter().filter(|r| r.success).count(),
            1,
            "the refused fetch is audited as a failure"
        );

        // The registry row: pending, the account, no password.
        let row = app.state.cluster.get(CLIENT).expect("registered");
        assert_eq!(row.registration_state, "pending");
        assert_eq!(row.gateway_eui.as_deref(), Some(EUI));
        assert_eq!(row.peer_username, format!("peer-{CLIENT}"));
        assert_eq!(row.registered_by.as_deref(), Some("admin"));
        assert_eq!(
            super::super::hub_routes::hub_meta(&app.state, CLIENT)
                .await
                .and_then(|m| m.name)
                .as_deref(),
            Some("Sklad B")
        );

        // The audit row, with both effects and no password.
        let rows = audit(&app.state, "hub_client_registered").await;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].success);
        assert_eq!(rows[0].target.as_deref(), Some(CLIENT));
        let details: Value = serde_json::from_str(rows[0].details.as_deref().unwrap()).unwrap();
        assert_eq!(details["account"], "ok");
        assert_eq!(details["gateway"], "ok");
        assert_eq!(details["registration_state"], "pending");
        assert!(details["request_id"].is_string());

        // Nowhere else, and neither is the token that fetched it.
        for entry in [rows, reveals].concat() {
            let details = entry.details.unwrap_or_default();
            assert!(!details.contains(PASSWORD));
            assert!(!details.contains(&token), "an audit row carried the token");
        }
        app.state.db.blocking(|_| ()); // let the writer settle
        assert!(
            !contains(&every_byte_on_disk(&app.state), PASSWORD),
            "the password reached a file under the data directory"
        );
        while let Ok(status) = status_rx.try_recv() {
            assert!(
                !format!("{status:?}").contains(PASSWORD),
                "SSE status event: {status:?}"
            );
        }
        drop(_guard);
        let logged = String::from_utf8_lossy(&logs.0.lock().unwrap()).to_string();
        assert!(logged.contains("capture-probe"), "the capture works");
        assert!(
            !logged.contains(PASSWORD),
            "a log line carried the password"
        );
        assert!(!logged.contains(&token), "a log line carried the token");
    }

    /// cluster.md §6: the Client hashes `GET /connection/ca` exactly as
    /// received, so `f` must be the SHA-256 of those bytes.
    #[tokio::test]
    async fn the_bundle_fingerprint_is_the_hash_of_what_the_ca_route_serves() {
        let app = app().await;
        let served = axum::body::to_bytes(
            send(&app, "GET", "/connection/ca", "", String::new())
                .await
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap();
        let expected = {
            use base64::Engine as _;
            use sha2::Digest as _;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(&served))
        };
        assert_eq!(expected.len(), 43);

        let html = text(
            post_answered(
                &app,
                &format!("/hubs/{SERVER}/cluster/clients"),
                format!("hostname={CLIENT}&gateway_eui={EUI}"),
                0,
                "SUCCESS",
                Some(add_result("ok")),
            )
            .await,
        )
        .await;
        let shown = text(reveal(&app, &app.admin, &token_in(&html)).await).await;
        assert!(
            shown.contains(&format!("&quot;f&quot;:&quot;{expected}&quot;")),
            "the bundle's f is the served CA's hash: {shown}"
        );
    }

    /// §4 step 3: a minted account whose gateway failed is "registration
    /// incomplete" — a `failed` row, the reason, Retry — and no bundle.
    #[tokio::test]
    async fn a_partial_registration_is_failed_and_shows_no_password() {
        let app = app().await;
        let html = text(
            post_answered(
                &app,
                &format!("/hubs/{SERVER}/cluster/clients"),
                format!("hostname={CLIENT}&gateway_eui={EUI}"),
                0,
                "ERROR",
                Some(add_result("failed")),
            )
            .await,
        )
        .await;
        assert!(
            !html.contains(PASSWORD),
            "no bundle for a half registration"
        );
        assert!(
            !html.contains("name=\"token\""),
            "and no token to fetch one with"
        );
        assert_eq!(app.state.reveals.held(), 0, "nothing is held for it");
        assert!(html.contains(&format!("/hubs/{SERVER}/cluster/clients/{CLIENT}/retry")));
        let row = app.state.cluster.get(CLIENT).unwrap();
        assert_eq!(row.registration_state, "failed");
        assert!(row.registration_error.unwrap().contains("gateway failed"));
        let rows = audit(&app.state, "hub_client_registered").await;
        assert!(!rows[0].success);
        assert!(!contains(&every_byte_on_disk(&app.state), PASSWORD));
    }

    /// An agent that answers with the password in clear (from before the seal)
    /// or with a seal that does not open: the registration is incomplete, no
    /// bundle is shown, and the reason says why.
    #[tokio::test]
    async fn a_clear_or_unopenable_password_is_never_shown() {
        let app = app().await;
        let uri = format!("/hubs/{SERVER}/cluster/clients");
        let request = send(
            &app,
            "POST",
            &uri,
            &app.admin,
            format!("hostname={CLIENT}&gateway_eui={EUI}"),
        );
        let (response, ()) = tokio::join!(
            request,
            answer_in_clear(&app, 0, "SUCCESS", Some(add_result("ok")))
        );
        let html = text(response).await;
        assert!(!html.contains(PASSWORD), "a clear password is not used");
        let row = app.state.cluster.get(CLIENT).unwrap();
        assert_eq!(row.registration_state, "failed");
        assert!(row.registration_error.unwrap().contains("in clear"));

        // A seal made for another key.
        let mut doc = add_result("ok");
        doc.as_object_mut().unwrap().remove("password");
        let stranger = fiber_protocol::sealed_reply::ReplySecret::generate();
        doc["password_sealed"] = serde_json::to_value(
            fiber_protocol::sealed_reply::seal(PASSWORD, &stranger.reply_key(), "x", CLIENT)
                .unwrap(),
        )
        .unwrap();
        let html = text(
            post_answered(
                &app,
                &format!("/hubs/{SERVER}/cluster/clients/{CLIENT}/retry"),
                String::new(),
                1,
                "SUCCESS",
                Some(doc),
            )
            .await,
        )
        .await;
        assert!(!html.contains(PASSWORD));
        let row = app.state.cluster.get(CLIENT).unwrap();
        assert_eq!(row.registration_state, "failed");
        assert!(row.registration_error.unwrap().contains("does not open"));
    }

    /// Retry runs the same operation, says the old password is dead, and a
    /// success moves the row back to pending.
    #[tokio::test]
    async fn retry_rotates_and_completes_an_incomplete_registration() {
        let app = app().await;
        post_answered(
            &app,
            &format!("/hubs/{SERVER}/cluster/clients"),
            format!("hostname={CLIENT}&gateway_eui={EUI}"),
            0,
            "ERROR",
            Some(add_result("failed")),
        )
        .await;
        let html = text(
            post_answered(
                &app,
                &format!("/hubs/{SERVER}/cluster/clients/{CLIENT}/retry"),
                String::new(),
                1,
                "SUCCESS",
                Some(add_result("ok")),
            )
            .await,
        )
        .await;
        assert!(!html.contains(PASSWORD));
        assert!(html.contains(&t("monitoring.hubs.bundle_rotated")));
        let shown = text(reveal(&app, &app.admin, &token_in(&html)).await).await;
        assert!(shown.contains(PASSWORD));
        let sent = requests(&app.state);
        assert_eq!(
            without_reply_key(&sent[1].1["params"]),
            json!({"hostname": CLIENT, "gateway_eui": EUI})
        );
        assert_ne!(
            sent[0].1["params"]["reply_key"], sent[1].1["params"]["reply_key"],
            "a fresh key for every request"
        );
        assert_eq!(
            app.state.cluster.get(CLIENT).unwrap().registration_state,
            "pending"
        );
        assert_eq!(
            audit(&app.state, "hub_client_registration_retry")
                .await
                .len(),
            1
        );
    }

    /// §8.3 and §4 step 6: every field is checked on the server, a refusal
    /// lands at its field with the typed values kept, and nothing is
    /// signed or stored.
    #[tokio::test]
    async fn a_refused_field_signs_and_stores_nothing() {
        let app = app().await;
        app.state.store.apply(IngestEvent::SystemInfo {
            hostname: SERVER.into(),
            payload: Box::new(
                serde_json::from_value(json!({"lorawan": {"gateway_present": true, "gateway_eui": "70b3d54900aa0001"}}))
                    .unwrap(),
            ),
        });
        for (body, error_key) in [
            (
                "hostname=hub-1a2b3c4d&gateway_eui=70b3d54900aa0002",
                "add_hub_err_hostname",
            ),
            (
                &*format!("hostname={SERVER}&gateway_eui=70b3d54900aa0002"),
                "add_hub_err_hostname_server",
            ),
            (
                "hostname=proximos-1a2b3c4d&gateway_eui=70b3d54900aa00",
                "add_hub_err_eui",
            ),
            (
                "hostname=proximos-1a2b3c4d&gateway_eui=70b3d54900aa0001",
                "add_hub_err_eui_server",
            ),
        ] {
            let html = text(
                send(
                    &app,
                    "POST",
                    &format!("/hubs/{SERVER}/cluster/clients"),
                    &app.admin,
                    body.to_string(),
                )
                .await,
            )
            .await;
            let sentence = t(&format!("monitoring.hubs.{error_key}"));
            assert!(
                html.contains(&sentence),
                "{body}: expected {error_key}: {html}"
            );
            assert!(html.contains("name=\"hostname\""), "the form comes back");
        }
        assert!(requests(&app.state).is_empty(), "nothing was signed");
        assert!(app.state.cluster.list().is_empty(), "nothing was stored");

        // A registered hostname or EUI is a duplicate at its field.
        post_answered(
            &app,
            &format!("/hubs/{SERVER}/cluster/clients"),
            format!("hostname={CLIENT}&gateway_eui={EUI}"),
            0,
            "SUCCESS",
            Some(add_result("ok")),
        )
        .await;
        for (body, error_key) in [
            (
                format!("hostname={CLIENT}&gateway_eui=70b3d54900aa0009"),
                "add_hub_err_hostname_taken",
            ),
            (
                format!("hostname=proximos-00000009&gateway_eui={EUI}"),
                "add_hub_err_eui_taken",
            ),
        ] {
            let html = text(
                send(
                    &app,
                    "POST",
                    &format!("/hubs/{SERVER}/cluster/clients"),
                    &app.admin,
                    body.clone(),
                )
                .await,
            )
            .await;
            let sentence = t(&format!("monitoring.hubs.{error_key}"));
            assert!(html.contains(&sentence), "{body}: {html}");
        }
        assert_eq!(requests(&app.state).len(), 1);
        assert_eq!(app.state.cluster.list().len(), 1);
    }

    /// §8.1: every route is admin; a manager or viewer gets 403 and nothing
    /// is signed. The pages render no Add-hub control for them either.
    #[tokio::test]
    async fn only_an_admin_can_add_retry_reveal_or_remove() {
        let app = app().await;
        post_answered(
            &app,
            &format!("/hubs/{SERVER}/cluster/clients"),
            format!("hostname={CLIENT}&gateway_eui={EUI}"),
            0,
            "SUCCESS",
            Some(add_result("ok")),
        )
        .await;
        for role in [Role::Manager, Role::Viewer] {
            let token = login_as_role(&app.state, role, &format!("{role:?}").to_lowercase()).await;
            for (uri, body) in [
                (
                    format!("/hubs/{SERVER}/cluster/clients"),
                    "hostname=proximos-00000009&gateway_eui=70b3d54900aa0009",
                ),
                (format!("/hubs/{SERVER}/cluster/clients/{CLIENT}/retry"), ""),
                (
                    format!("/hubs/{SERVER}/cluster/clients/{CLIENT}/reveal"),
                    "token=x",
                ),
                (format!("/hubs/{CLIENT}/cluster/remove"), "reason=test"),
            ] {
                let response = send(&app, "POST", &uri, &token, body.to_string()).await;
                assert_eq!(response.status(), StatusCode::FORBIDDEN, "{role:?} {uri}");
            }
            let list = text(send(&app, "GET", "/hubs", &token, String::new()).await).await;
            assert!(!list.contains("add-hub-dialog"), "{role:?} sees no Add hub");
            let system = text(
                send(
                    &app,
                    "GET",
                    &format!("/hubs/{SERVER}/system"),
                    &token,
                    String::new(),
                )
                .await,
            )
            .await;
            assert!(!system.contains("add-hub-dialog"));
            let client = text(
                send(
                    &app,
                    "GET",
                    &format!("/hubs/{CLIENT}/system"),
                    &token,
                    String::new(),
                )
                .await,
            )
            .await;
            assert!(
                !client.contains("remove-client-dialog"),
                "{role:?} sees no removal"
            );
        }
        assert_eq!(requests(&app.state).len(), 1, "no request but the admin's");
        assert!(audit(&app.state, "hub_peer_secret_revealed")
            .await
            .is_empty());

        let list = text(send(&app, "GET", "/hubs", &app.admin, String::new()).await).await;
        assert!(list.contains("add-hub-dialog"), "the admin does");
    }

    /// §12 C: `known_hub()` is store ∪ registry. A Client that has never
    /// connected has a list row and all four tabs; an unknown hostname is
    /// still a 404.
    #[tokio::test]
    async fn a_registered_client_is_a_known_hub_before_it_ever_connects() {
        let app = app().await;
        app.state
            .cluster
            .insert(portal_storage::NewClusterClient {
                hostname: CLIENT.into(),
                gateway_eui: Some(EUI.into()),
                peer_username: format!("peer-{CLIENT}"),
                registered_by: Some("admin".into()),
                registration_error: "x".into(),
            })
            .await
            .unwrap();
        app.state
            .cluster
            .record_result(CLIENT, &format!("peer-{CLIENT}"), None)
            .await
            .unwrap();
        assert!(
            app.state.store.snapshot().device(CLIENT).is_none(),
            "not in the store"
        );

        for tab in ["overview", "sensors", "unit", "system"] {
            let response = send(
                &app,
                "GET",
                &format!("/hubs/{}/{tab}", CLIENT.to_uppercase()),
                &app.admin,
                String::new(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "{tab}");
            let html = text(response).await;
            assert!(
                html.contains(&t("monitoring.hubs.client_never_connected")),
                "{tab}"
            );
        }
        let system = text(
            send(
                &app,
                "GET",
                &format!("/hubs/{CLIENT}/system"),
                &app.admin,
                String::new(),
            )
            .await,
        )
        .await;
        assert!(
            system.contains("remove-client-dialog"),
            "the admin can remove it"
        );

        let list = text(send(&app, "GET", "/hubs", &app.admin, String::new()).await).await;
        assert!(
            list.contains(&format!("id=\"hub-{CLIENT}\"")),
            "a row from the registry alone"
        );
        assert!(list.contains(&t("monitoring.hubs.registered_waiting")));
        assert!(
            list.contains(&t("monitoring.hubs.kind_client")),
            "Typ shows in a mixed list"
        );
        let server_at = list.find(&format!("id=\"hub-{SERVER}\"")).unwrap();
        let client_at = list.find(&format!("id=\"hub-{CLIENT}\"")).unwrap();
        assert!(server_at < client_at, "the Server first (§2.2)");

        let server = text(
            send(
                &app,
                "GET",
                &format!("/hubs/{SERVER}/system"),
                &app.admin,
                String::new(),
            )
            .await,
        )
        .await;
        assert!(
            server.contains(&format!("/hubs/{CLIENT}/system")),
            "the cluster card lists it"
        );

        assert_eq!(
            send(
                &app,
                "GET",
                "/hubs/proximos-00000009/overview",
                &app.admin,
                String::new()
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }

    /// HIL 2026-10-13 (cluster.md amendment of that date): a Client registered
    /// without a gateway that then runs LoRaWAN has its gateway publishes
    /// dropped by the Server's ACL. The Client's row, its page and its row in
    /// the Server's cluster card say so; the Server's own row and pages do not
    /// (amendment 2026-10-28); a Client in P2P says nothing.
    #[tokio::test]
    async fn a_client_gateway_the_server_drops_is_shown_on_the_client() {
        use fiber_protocol::payloads::system_info::{LorawanGatewayInfo, SystemInfoMsg};

        let app = app().await;
        app.state
            .cluster
            .insert(portal_storage::NewClusterClient {
                hostname: CLIENT.into(),
                gateway_eui: None,
                peer_username: format!("peer-{CLIENT}"),
                registered_by: Some("admin".into()),
                registration_error: "x".into(),
            })
            .await
            .unwrap();
        app.state
            .cluster
            .record_result(CLIENT, &format!("peer-{CLIENT}"), None)
            .await
            .unwrap();
        let report = |mode: &str| {
            app.state.store.apply(IngestEvent::SystemInfo {
                hostname: CLIENT.into(),
                payload: Box::new(SystemInfoMsg {
                    lorawan: Some(LorawanGatewayInfo {
                        mode: Some(mode.into()),
                        concentratord_running: Some(true),
                        ..Default::default()
                    }),
                    ..SystemInfoMsg::default()
                }),
            });
        };
        let get = |path: String| {
            let app = &app;
            async move { text(send(app, "GET", &path, &app.admin, String::new()).await).await }
        };
        let unregistered = t("monitoring.hubs.client_gateway_unregistered");

        // Joined in P2P: no forwarder, no gap.
        report("p2p");
        let list = get("/hubs".into()).await;
        assert!(!list.contains("data-gateway-gap"), "{list}");
        let server = get(format!("/hubs/{SERVER}/overview")).await;
        assert!(!server.contains("data-gateway-gap"));

        // The card arrived and the Client runs LoRaWAN.
        report("dual");
        let list = get("/hubs".into()).await;
        assert!(
            list.contains("data-gateway-gap=\"unregistered\""),
            "the Client's row"
        );
        assert!(
            !list.contains("data-gateway-gap=\"server\""),
            "not the Server's row"
        );
        assert!(list.contains(&t("monitoring.hubs.gateway_gap_badge")));
        let client = get(format!("/hubs/{CLIENT}/overview")).await;
        assert!(client.contains(&unregistered), "the Client's page");
        for tab in ["overview", "system"] {
            let server = get(format!("/hubs/{SERVER}/{tab}")).await;
            assert!(
                !server.contains("alert alert-warning\" data-gateway-gap"),
                "{tab}"
            );
        }
        let card = get(format!("/hubs/{SERVER}/system")).await;
        assert!(
            card.contains("data-gateway-gap=\"unregistered\""),
            "the cluster card"
        );
    }

    /// cluster.md §9: `pending` turns `complete` when the Client's status
    /// reaches this broker; a `failed` row does not move.
    #[tokio::test]
    async fn a_pending_client_completes_when_its_status_arrives() {
        let app = app().await;
        for (hostname, eui, ok) in [
            (CLIENT, EUI, true),
            ("proximos-00000002", "70b3d54900aa0003", false),
        ] {
            app.state
                .cluster
                .insert(portal_storage::NewClusterClient {
                    hostname: hostname.into(),
                    gateway_eui: Some(eui.into()),
                    peer_username: format!("peer-{hostname}"),
                    registered_by: None,
                    registration_error: "x".into(),
                })
                .await
                .unwrap();
            app.state
                .cluster
                .record_result(
                    hostname,
                    &format!("peer-{hostname}"),
                    (!ok).then_some("gateway failed"),
                )
                .await
                .unwrap();
        }
        tokio::spawn(crate::cluster::watch_liveness(
            app.state.store.clone(),
            app.state.cluster.clone(),
        ));
        tokio::task::yield_now().await;
        for hostname in [CLIENT, "proximos-00000002"] {
            app.state.store.apply(IngestEvent::Status {
                hostname: hostname.to_uppercase(),
                status: DeviceStatus::Online,
            });
        }
        for _ in 0..200 {
            if app.state.cluster.get(CLIENT).unwrap().registration_state == "complete" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let row = app.state.cluster.get(CLIENT).unwrap();
        assert_eq!(row.registration_state, "complete");
        assert!(row.completed_at.is_some());
        assert_eq!(
            app.state
                .cluster
                .get("proximos-00000002")
                .unwrap()
                .registration_state,
            "failed"
        );
    }

    /// Decision of 2026-10-02: a Retry rotates the password, so the token of
    /// the bundle before it reveals nothing.
    #[tokio::test]
    async fn a_retry_invalidates_the_outstanding_token() {
        let app = app().await;
        let first = text(
            post_answered(
                &app,
                &format!("/hubs/{SERVER}/cluster/clients"),
                format!("hostname={CLIENT}&gateway_eui={EUI}"),
                0,
                "SUCCESS",
                Some(add_result("ok")),
            )
            .await,
        )
        .await;
        let second = text(
            post_answered(
                &app,
                &format!("/hubs/{SERVER}/cluster/clients/{CLIENT}/retry"),
                String::new(),
                1,
                "SUCCESS",
                Some(add_result("ok")),
            )
            .await,
        )
        .await;
        let (old, new) = (token_in(&first), token_in(&second));
        assert_ne!(old, new);
        assert_eq!(app.state.reveals.held(), 1, "one bundle per Client");
        let stale = text(reveal(&app, &app.admin, &old).await).await;
        assert!(!stale.contains(PASSWORD));
        assert!(stale.contains(&t("monitoring.hubs.bundle_gone_title")));
        assert!(text(reveal(&app, &app.admin, &new).await)
            .await
            .contains(PASSWORD));
    }

    /// A Retry (or "New password") that never reaches the agent rotates
    /// nothing, so the Client's row keeps the state it had: a working
    /// `complete` Client is not marked "registration incomplete" because the
    /// broker was down for the click.
    #[tokio::test]
    async fn a_retry_that_never_leaves_the_portal_keeps_the_row_as_it_was() {
        let app = app_with(
            crate::cluster::REVEAL_TTL,
            Some(crate::relay::CommandRelay::broker_down_instance()),
        )
        .await;
        app.state
            .cluster
            .insert(portal_storage::NewClusterClient {
                hostname: CLIENT.into(),
                gateway_eui: Some(EUI.into()),
                peer_username: format!("peer-{CLIENT}"),
                registered_by: Some("admin".into()),
                registration_error: "registration did not finish".into(),
            })
            .await
            .unwrap();
        app.state
            .cluster
            .record_result(CLIENT, &format!("peer-{CLIENT}"), None)
            .await
            .unwrap();
        assert!(app.state.cluster.mark_complete(CLIENT).await.unwrap());
        let before = app.state.cluster.get(CLIENT).unwrap();

        let html = text(
            send(
                &app,
                "POST",
                &format!("/hubs/{SERVER}/cluster/clients/{CLIENT}/retry"),
                &app.admin,
                String::new(),
            )
            .await,
        )
        .await;
        assert!(!html.contains("name=\"token\""), "no bundle: {html}");
        assert!(
            html.contains(&t("monitoring.command_error.mqtt_unavailable")),
            "{html}"
        );
        assert_eq!(app.state.cluster.get(CLIENT).unwrap(), before);
        let stored = app.state.db.run(|s| s.get_cluster_client(CLIENT)).await;
        assert_eq!(stored.unwrap(), Some(before), "the database too");
    }

    /// #164: Add hub takes no EUI on a LoRaWAN Server either; the Client reports it later and
    /// the admin grants it with `cluster-client-gateway`, which keeps the password.
    #[tokio::test]
    async fn a_reported_gateway_is_granted_by_the_admin() {
        let app = app().await;
        post_answered(
            &app,
            &format!("/hubs/{SERVER}/cluster/clients"),
            format!("hostname={CLIENT}&gateway_eui="),
            0,
            "SUCCESS",
            Some(add_result("ok")),
        )
        .await;
        let sent = requests(&app.state);
        assert_eq!(
            without_reply_key(&sent[0].1["params"]),
            json!({"hostname": CLIENT})
        );
        assert_eq!(app.state.cluster.get(CLIENT).unwrap().gateway_eui, None);

        // Malformed or another Client's: refused, nothing signed.
        let html = text(
            send(
                &app,
                "POST",
                &format!("/hubs/{CLIENT}/cluster/gateway"),
                &app.admin,
                "gateway_eui=70b3".into(),
            )
            .await,
        )
        .await;
        assert!(
            html.contains(&t("monitoring.hubs.add_hub_err_eui")),
            "{html}"
        );
        assert_eq!(requests(&app.state).len(), 1);

        post_answered(
            &app,
            &format!("/hubs/{CLIENT}/cluster/gateway"),
            format!("gateway_eui={EUI}"),
            1,
            "SUCCESS",
            Some(json!({"hostname": CLIENT, "gateway_eui": EUI, "account": "ok", "gateway": "ok"})),
        )
        .await;
        let sent = requests(&app.state);
        assert_eq!(sent[1].1["command_type"], "cluster-client-gateway");
        assert_eq!(
            sent[1].1["params"],
            json!({"hostname": CLIENT, "gateway_eui": EUI})
        );
        assert_eq!(
            app.state
                .cluster
                .get(CLIENT)
                .unwrap()
                .gateway_eui
                .as_deref(),
            Some(EUI)
        );
        assert_eq!(
            audit(&app.state, "hub_client_gateway_granted").await.len(),
            1
        );
    }

    /// #173: Retry asks for no EUI; a submitted one is ignored and the row's
    /// registered gateway is kept.
    #[tokio::test]
    async fn retry_keeps_the_registered_gateway_eui() {
        let app = app().await;
        post_answered(
            &app,
            &format!("/hubs/{SERVER}/cluster/clients"),
            format!("hostname={CLIENT}&gateway_eui={EUI}"),
            0,
            "ERROR",
            Some(add_result("failed")),
        )
        .await;
        let registered = app.state.cluster.get(CLIENT).unwrap().gateway_eui;
        let retry = format!("/hubs/{SERVER}/cluster/clients/{CLIENT}/retry");
        let form = text(send(&app, "GET", &retry, &app.admin, String::new()).await).await;
        assert!(!form.contains("name=\"gateway_eui\""), "{form}");

        post_answered(
            &app,
            &retry,
            "gateway_eui=70:B3:D5:49:00:AA:00:09".into(),
            1,
            "SUCCESS",
            Some(add_result("ok")),
        )
        .await;
        let sent = requests(&app.state);
        assert_eq!(sent[1].1["command_type"], "cluster-client-add");
        assert_ne!(
            sent[1].1["params"]["gateway_eui"],
            json!("70b3d54900aa0009"),
            "the submitted EUI is ignored"
        );
        assert_eq!(
            app.state.cluster.get(CLIENT).unwrap().gateway_eui,
            registered
        );
    }

    /// A Remove that deletes the row while `cluster-client-add` is out: the
    /// answer finds no row, so no bundle and no token are issued, and the
    /// screen says the registration was removed meanwhile.
    #[tokio::test]
    async fn a_row_removed_meanwhile_gets_no_bundle() {
        let app = app().await;
        let uri = format!("/hubs/{SERVER}/cluster/clients");
        let request = send(
            &app,
            "POST",
            &uri,
            &app.admin,
            format!("hostname={CLIENT}&gateway_eui={EUI}"),
        );
        let remove_then_answer = async {
            while requests(&app.state).is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(app.state.cluster.delete(CLIENT).await.unwrap());
            answer(&app, 0, "SUCCESS", Some(add_result("ok"))).await;
        };
        let (response, ()) = tokio::join!(request, remove_then_answer);
        let html = text(response).await;
        assert!(!html.contains("name=\"token\""), "no token: {html}");
        assert!(!html.contains(PASSWORD));
        assert!(
            html.contains(&t("monitoring.hubs.add_hub_err_removed")),
            "{html}"
        );
        assert_eq!(app.state.reveals.held(), 0, "nothing in the vault");
        assert!(app.state.cluster.get(CLIENT).is_none(), "no row comes back");
        let rows = audit(&app.state, "hub_client_registered").await;
        assert!(rows[0]
            .details
            .as_deref()
            .unwrap()
            .contains("\"registration_state\":\"removed\""));
    }

    /// The token is bound to the admin's session: another session, even the
    /// same admin's, is refused, and the token is burnt.
    #[tokio::test]
    async fn another_sessions_token_is_refused() {
        let app = app().await;
        let html = text(
            post_answered(
                &app,
                &format!("/hubs/{SERVER}/cluster/clients"),
                format!("hostname={CLIENT}&gateway_eui={EUI}"),
                0,
                "SUCCESS",
                Some(add_result("ok")),
            )
            .await,
        )
        .await;
        let token = token_in(&html);
        let other = login(&app.state, "admin", "correct-horse-battery").await;
        assert_ne!(other, app.admin, "a second session");
        let refused = text(reveal(&app, &other, &token).await).await;
        assert!(!refused.contains(PASSWORD), "{refused}");
        assert!(refused.contains(&t("monitoring.hubs.bundle_gone_other_session")));
        let rows = audit(&app.state, "hub_peer_secret_revealed").await;
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].success);
        assert!(rows[0]
            .details
            .as_deref()
            .unwrap()
            .contains("other_session"));
        assert!(
            !text(reveal(&app, &app.admin, &token).await)
                .await
                .contains(PASSWORD),
            "burnt for its owner too"
        );
    }

    /// The bundle is held about five minutes; past that the token reveals
    /// nothing, and the bundle has left memory without anybody asking.
    #[tokio::test]
    async fn an_unrevealed_bundle_expires() {
        let app = app_with_ttl(std::time::Duration::from_millis(100)).await;
        let html = text(
            post_answered(
                &app,
                &format!("/hubs/{SERVER}/cluster/clients"),
                format!("hostname={CLIENT}&gateway_eui={EUI}"),
                0,
                "SUCCESS",
                Some(add_result("ok")),
            )
            .await,
        )
        .await;
        assert_eq!(app.state.reveals.held(), 1);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(app.state.reveals.held(), 0, "swept");
        let late = text(reveal(&app, &app.admin, &token_in(&html)).await).await;
        assert!(!late.contains(PASSWORD));
        assert!(late.contains(&t("monitoring.hubs.bundle_gone_body")));
        assert_eq!(crate::cluster::REVEAL_TTL.as_secs(), 300);
    }

    /// An unpaired Client cannot be reset, so its removal needs the operator's explicit
    /// acceptance; without it nothing is signed. With it, only the Server's half runs.
    #[tokio::test]
    async fn an_unpaired_client_is_removed_only_when_the_missing_reset_is_accepted() {
        let app = app().await;
        post_answered(
            &app,
            &format!("/hubs/{SERVER}/cluster/clients"),
            format!("hostname={CLIENT}&gateway_eui={EUI}"),
            0,
            "SUCCESS",
            Some(add_result("ok")),
        )
        .await;

        let refused = text(
            send(
                &app,
                "POST",
                &format!("/hubs/{CLIENT}/cluster/remove"),
                &app.admin,
                format!("reason=x&confirm_hostname={CLIENT}"),
            )
            .await,
        )
        .await;
        assert!(
            refused.contains(&t("monitoring.hubs.remove_unpaired_not_accepted")),
            "{refused}"
        );
        assert_eq!(requests(&app.state).len(), 1, "no refusal was signed");

        let html = text(
            post_answered(
                &app,
                &format!("/hubs/{CLIENT}/cluster/remove"),
                format!("reason=x&confirm_hostname={CLIENT}&accept_no_reset=1"),
                1,
                "SUCCESS",
                Some(json!({
                    "hostname": CLIENT, "username": format!("peer-{CLIENT}"), "gateway_eui": EUI,
                    "account": "ok", "gateway": "ok", "message": "removed",
                })),
            )
            .await,
        )
        .await;
        assert!(
            html.contains(&t("monitoring.hubs.effect_reset_skipped")),
            "{html}"
        );
        assert_eq!(requests(&app.state).len(), 2, "no reset was signed");
    }

    /// §5: a complete removal drops the pairing keys and the row; an
    /// incomplete one keeps the row and says so. Each effect is reported.
    #[tokio::test]
    async fn removal_reports_each_effect_and_deletes_only_when_complete() {
        let app = app().await;
        let added = text(
            post_answered(
                &app,
                &format!("/hubs/{SERVER}/cluster/clients"),
                format!("hostname={CLIENT}&gateway_eui={EUI}"),
                0,
                "SUCCESS",
                Some(add_result("ok")),
            )
            .await,
        )
        .await;
        let token = token_in(&added);
        app.state
            .keys
            .store_pairing(
                CLIENT,
                Some("client-ca".into()),
                "11".repeat(32),
                [8u8; 32],
                None,
                None,
            )
            .await
            .unwrap();

        let refused = send(
            &app,
            "POST",
            &format!("/hubs/{CLIENT}/cluster/remove"),
            &app.admin,
            "reason=".into(),
        )
        .await;
        assert!(text(refused)
            .await
            .contains(&t("monitoring.hubs.remove_reason_required")));
        let server = text(
            send(
                &app,
                "POST",
                &format!("/hubs/{SERVER}/cluster/remove"),
                &app.admin,
                "reason=x".into(),
            )
            .await,
        )
        .await;
        assert!(
            server.contains(&t("monitoring.hubs.remove_not_client")),
            "{server}"
        );
        let mismatch = text(
            send(
                &app,
                "POST",
                &format!("/hubs/{CLIENT}/cluster/remove"),
                &app.admin,
                "reason=x&confirm_hostname=proximos-00000000".into(),
            )
            .await,
        )
        .await;
        assert!(
            mismatch.contains(&t("monitoring.hubs.remove_hostname_mismatch")),
            "{mismatch}"
        );
        assert_eq!(requests(&app.state).len(), 1, "no refusal was signed");
        assert!(audit(&app.state, "hub_client_removed").await.is_empty());

        let remove_result = |gateway: &str| {
            json!({
                "hostname": CLIENT, "username": format!("peer-{CLIENT}"), "gateway_eui": EUI,
                "account": "ok", "gateway": gateway, "message": format!("gateway {gateway}"),
            })
        };
        let html = text(
            remove_answered(
                &app,
                format!("reason=moved+to+site+B&confirm_hostname={CLIENT}"),
                1,
                "SUCCESS",
                "ERROR",
                Some(remove_result("failed")),
            )
            .await,
        )
        .await;
        assert!(
            html.contains(&t("monitoring.hubs.remove_incomplete")),
            "{html}"
        );
        let reset = &requests(&app.state)[1];
        assert_eq!(
            reset.1["command_type"], "factory_reset",
            "the Client first (#171)"
        );
        assert_eq!(reset.1["params"], json!({"post_action": "reboot"}));
        let sent = &requests(&app.state)[2];
        assert_eq!(
            sent.0, "proximos/proximos-a1b2c3d4/commands/config/request",
            "to the Server"
        );
        assert_eq!(sent.1["command_type"], "cluster-client-remove");
        assert_eq!(sent.1["params"], json!({"hostname": CLIENT}));
        let row = app.state.cluster.get(CLIENT).expect("kept");
        assert!(row.removal_error.is_some());
        assert!(
            matches!(app.state.keys.device_identity(CLIENT).await, Ok(Some(_))),
            "keys kept"
        );
        // Even an incomplete removal ends the account, so the unrevealed
        // bundle went with it.
        assert_eq!(app.state.reveals.held(), 0);
        assert!(!text(reveal(&app, &app.admin, &token).await)
            .await
            .contains(PASSWORD));

        // #168: the Client has published on the broker, so the store knows it.
        app.state.store.apply(IngestEvent::SystemInfo {
            hostname: CLIENT.into(),
            payload: Box::new(fiber_protocol::payloads::system_info::SystemInfoMsg::default()),
        });
        assert!(app.state.store.snapshot_device(CLIENT).is_some());

        let html = text(
            remove_answered(
                &app,
                format!("reason=moved+to+site+B&confirm_hostname={CLIENT}"),
                3,
                "SUCCESS",
                "SUCCESS",
                Some(remove_result("ok")),
            )
            .await,
        )
        .await;
        assert!(html.contains(&t("monitoring.hubs.removed")), "{html}");
        assert!(app.state.cluster.get(CLIENT).is_none(), "the row is gone");
        assert!(
            matches!(app.state.keys.device_identity(CLIENT).await, Ok(None)),
            "keys dropped"
        );
        let rows = audit(&app.state, "hub_client_removed").await;
        assert_eq!(rows.len(), 2, "{rows:#?}");
        let details: Vec<Value> = rows
            .iter()
            .map(|r| serde_json::from_str(r.details.as_deref().unwrap()).unwrap())
            .collect();
        assert!(details.iter().any(|d| d["gateway"] == "failed"));
        assert!(details.iter().all(|d| d["reason"] == "moved to site B"));
        assert!(details.iter().all(|d| d["factory_reset"] == "done"));
        assert_eq!(
            send(
                &app,
                "GET",
                &format!("/hubs/{CLIENT}/overview"),
                &app.admin,
                String::new()
            )
            .await
            .status(),
            StatusCode::NOT_FOUND,
            "a removed Client is no longer a known Hub, though it had published (#168)"
        );
        assert!(
            app.state.store.snapshot_device(CLIENT).is_none(),
            "forgotten"
        );
    }

    /// C2's known gap: an admin key minted before `cluster.manage` existed
    /// lacks it. `ensure_user_key` re-mints with the union, so Add hub
    /// signs with a certificate that carries it rather than failing.
    #[tokio::test]
    async fn an_admin_key_minted_before_cluster_manage_is_reminted_with_it() {
        let app = app().await;
        let admin = app.state.auth.validate(&app.admin).await.unwrap();
        let old: Vec<String> = crate::perms::permissions_for(&admin)
            .into_iter()
            .filter(|p| p != "cluster.manage")
            .collect();
        let stale = app
            .state
            .keys
            .ensure_user_key(&admin, SERVER, &old)
            .await
            .unwrap();
        assert!(!stale.certificate.to_string().contains("cluster.manage"));

        post_answered(
            &app,
            &format!("/hubs/{SERVER}/cluster/clients"),
            format!("hostname={CLIENT}&gateway_eui={EUI}"),
            0,
            "SUCCESS",
            Some(add_result("ok")),
        )
        .await;
        let sent = &requests(&app.state)[0].1;
        assert!(
            sent["certificate"]
                .to_string()
                .contains("\"cluster.manage\""),
            "the request rides a certificate with the verb's permission: {sent}"
        );
        assert_eq!(
            app.state.cluster.get(CLIENT).unwrap().registration_state,
            "pending"
        );
    }

    /// The dialog says why a Client cannot be added instead of hiding the
    /// button (§2.6, §4 preconditions), and the POST refuses the same way.
    #[tokio::test]
    async fn the_preconditions_are_explained_and_enforced() {
        let app = app().await;
        std::fs::remove_file(app.state.ca_cert_path.as_ref()).unwrap();
        let list = text(send(&app, "GET", "/hubs", &app.admin, String::new()).await).await;
        assert!(list.contains("add-hub-dialog"), "the button stays");
        assert!(list.contains(&t("monitoring.hubs.add_hub_no_ca")));
        let html = text(
            send(
                &app,
                "POST",
                &format!("/hubs/{SERVER}/cluster/clients"),
                &app.admin,
                format!("hostname={CLIENT}&gateway_eui={EUI}"),
            )
            .await,
        )
        .await;
        assert!(html.contains(&t("monitoring.hubs.add_hub_no_ca")));
        assert!(requests(&app.state).is_empty());
        assert!(app.state.cluster.list().is_empty());
    }
}
