//! One signed device command, from a route handler's point of view.
//!
//! Every mutating Node, Beacon and Hub route used to spell out the same
//! ritual inline — refuse early with a synthetic error outcome, mint the
//! caller's signing key, run the registry's certificate preflight, relay,
//! merge the outcome's status into the audit row, render `#command-status`
//! — and four helpers had each grown a private copy of the middle of it
//! (`relay_signed`, `run_system_command`, `relay_service_command`,
//! `signing_key`). [`Cmd`] is the one copy. A handler names the command
//! once, and every refusal, relay and audit row after that is a method on
//! it, so the rules below hold everywhere by construction rather than by
//! comment:
//!
//! - a refusal before anything was signed renders `partials/command_status.
//!   html` with `request_id: None` (`CommandOutcome`'s own contract);
//! - the audit row carries the outcome's real `status`, and `success` is
//!   `"success"` alone — since P4 every verb is a signed `config_request`
//!   with an ack, so there is no "sent and never answered" success left to
//!   record (plan/portal/mqtt/design.md §7);
//! - the certificate preflight runs for every command in the registry —
//!   universal now that every verb has a permission (design.md §4.4 step
//!   3), not only the four Hub system commands that first needed it.
//!
//! Layered on purpose: [`Cmd::execute`] stops at the outcome, for the
//! handlers that do portal-side work between the relay and the audit row
//! (a registration writes `device_meta`, a removal drops the draft);
//! [`Cmd::relay`] is execute + audit + render for the ones that do not.

use axum::response::Response;
use minijinja::context;
use serde_json::Value;

use crate::auth::CurrentUser;
use crate::http::AppState;
use crate::perms;
use crate::radio::RadioError;
use crate::relay::CommandOutcome;
use crate::render;

use super::{render_fragment, tr};

/// What a user reads when no signing key could be minted. A missing pairing
/// is the common cause and has a fix the user can act on, so it is named.
pub(crate) fn signing_key_error(err: &crate::keys::KeyError) -> String {
    match err {
        crate::keys::KeyError::NotPaired(_) => tr("monitoring.shared.signing_key_not_paired"),
        _ => tr("monitoring.shared.signing_key_failed"),
    }
}

/// The sentence for a code [`Cmd::execute_or_code`] refused with, for the page a full-page
/// flow redirects to; `None` for any other code, which the page then words itself. Shared, so
/// that "the Hub is not paired" reads the same after a redirect as in `#command-status`
/// (GitLab #109: an unpaired Hub's Add Beacon said "check the MQTT connection").
pub(crate) fn refusal_message(code: &str) -> Option<String> {
    let key = match code {
        REFUSED_NOT_PAIRED => "monitoring.shared.signing_key_not_paired",
        REFUSED_SIGNING_KEY => "monitoring.shared.signing_key_failed",
        REFUSED_NOT_PERMITTED => "monitoring.command_error.not_permitted",
        _ => return None,
    };
    Some(tr(key))
}

/// The `?error=` codes of [`Cmd::execute_or_code`].
const REFUSED_NOT_PAIRED: &str = "not_paired";
const REFUSED_SIGNING_KEY: &str = "signing_key_failed";
const REFUSED_NOT_PERMITTED: &str = "not_permitted";

/// Why [`Cmd::execute`] stopped before anything was signed.
enum Refusal {
    /// The caller's certificate would not carry the verb's permission.
    Permission(&'static str),
    /// No signing key could be minted — most often because the Hub is not paired.
    Key(crate::keys::KeyError),
}

impl Refusal {
    fn sentence(&self) -> String {
        match self {
            Refusal::Permission(permission) => {
                tr("monitoring.shared.missing_certificate_permission")
                    .replace("{permission}", permission)
            }
            Refusal::Key(err) => signing_key_error(err),
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Refusal::Permission(_) => REFUSED_NOT_PERMITTED,
            Refusal::Key(crate::keys::KeyError::NotPaired(_)) => REFUSED_NOT_PAIRED,
            Refusal::Key(_) => REFUSED_SIGNING_KEY,
        }
    }
}

pub(crate) struct Cmd<'a> {
    state: &'a AppState,
    user: &'a CurrentUser,
    /// The registry `command_type` — what the outcome names and what the
    /// relay publishes.
    command: &'a str,
    /// The name of the device the last [`Self::try_send`] was about (GitLab #121), for
    /// the banner's first line. Resolved there because that path is async and `done` is not.
    target: std::sync::Mutex<Option<String>>,
}

impl<'a> Cmd<'a> {
    pub(crate) fn new(state: &'a AppState, user: &'a CurrentUser, command: &'a str) -> Self {
        Self {
            state,
            user,
            command,
            target: std::sync::Mutex::new(None),
        }
    }

    /// Names the device this command is about up front (GitLab #121), for a refusal
    /// rendered before [`Self::try_send`] could read it off the params.
    pub(crate) fn on(self, id: &str) -> Self {
        let label = target_label_sync(self.state, id).unwrap_or_else(|| id.to_string());
        if let Ok(mut slot) = self.target.lock() {
            *slot = Some(label);
        }
        self
    }

    /// A refusal caught before anything was signed or published: a
    /// validation failure, an unknown device, a missing precondition.
    pub(crate) fn fail(&self, message: &str) -> Response {
        self.done(self.error(message))
    }

    /// A success that never went to the device — a draft written locally.
    pub(crate) fn ok(&self, message: &str) -> Response {
        self.done(CommandOutcome {
            request_id: None,
            command_type: self.command.to_string(),
            status: "success",
            message: Some(message.to_string()),
            error_code: None,
            result: None,
        })
    }

    /// Renders the outcome of a write the Portal has already stored as its own copy
    /// (decision #41: limits). A device that refused, timed out or could not be reached
    /// does not undo the save, so the answer says "saved in the Portal, the device copy
    /// was not delivered" with the device's reason as the detail, not a bare failure.
    pub(crate) fn done_stored(&self, outcome: CommandOutcome) -> Response {
        if succeeded(&outcome) {
            return self.done(outcome);
        }
        self.done(CommandOutcome {
            status: "stored",
            error_code: Some("device_copy_not_delivered"),
            ..outcome
        })
    }

    /// Renders any outcome as the `#command-status` fragment.
    ///
    /// `render_fragment`, not `render`: the partial draws no layout, so the
    /// stats tiles it cannot show would be a `PortalState` clone and a pass
    /// over every entity paid per command.
    pub(crate) fn done(&self, outcome: CommandOutcome) -> Response {
        let ctx = render::page_ctx(context! { cmd => self.ctx(outcome) }, Some(self.user));
        render_fragment(self.state, "partials/command_status.html", ctx)
    }

    /// `partials/command_status.html`'s `cmd` for `outcome`, with the device's name
    /// (GitLab #121) — for a caller that renders the partial inside its own fragment.
    pub(crate) fn ctx(&self, outcome: CommandOutcome) -> render::CommandOutcomeCtx {
        let target = self.target.lock().ok().and_then(|t| t.clone()).or_else(|| {
            let id = self.state.relay.target_of(outcome.request_id.as_deref()?)?;
            Some(target_label_sync(self.state, &id).unwrap_or(id))
        });
        let mut cmd = render::CommandOutcomeCtx::from(outcome);
        cmd.target = target;
        cmd
    }

    /// The synthetic error outcome behind [`Self::fail`], for a caller that
    /// wants to decide how to render it (a redirect instead of a fragment).
    pub(crate) fn error(&self, message: &str) -> CommandOutcome {
        CommandOutcome {
            request_id: None,
            command_type: self.command.to_string(),
            status: "error",
            message: Some(message.to_string()),
            error_code: None,
            result: None,
        }
    }

    /// Preflight, mint the caller's signing key, relay. `Err` is the
    /// outcome to show for a refusal on this side of the wire; `Ok` is
    /// whatever the device (or the relay's timeout) answered.
    ///
    /// The certificate preflight can only trip if a route's role gate and
    /// `perms::permissions_for` have drifted apart — it runs because that is
    /// the documented contract, not because it is expected to.
    pub(crate) async fn execute(
        &self,
        hostname: &str,
        params: Value,
        reason: Option<String>,
    ) -> Result<CommandOutcome, CommandOutcome> {
        self.try_execute(hostname, params, reason)
            .await
            .map_err(|refusal| self.error(&refusal.sentence()))
    }

    /// [`Self::execute`] for a full-page POST that answers with a redirect rather than a
    /// fragment: a refusal before anything was signed comes back as an `?error=` code the
    /// landing page turns into words with [`refusal_message`] — `not_paired` above all, which
    /// such a flow used to fold into its generic "failed" code (GitLab #109).
    ///
    /// Not audited, like every refusal from `execute` ([`Self::relay`]): nothing was signed.
    pub(crate) async fn execute_or_code(
        &self,
        hostname: &str,
        params: Value,
        reason: Option<String>,
    ) -> Result<CommandOutcome, &'static str> {
        self.try_execute(hostname, params, reason)
            .await
            .map_err(|refusal| refusal.code())
    }

    /// The write path of a value the Portal has ALREADY stored as its own copy (decision
    /// #41: limits). Signs and publishes without waiting for the device, audits, and answers
    /// at once with status `"saved"`: the Portal grades from its copy from now on. The
    /// device's answer follows on the SSE stream, where `sse.rs` words a failure of these
    /// verbs as "device copy not delivered" ([`stored_first`]), never as a failed save.
    pub(crate) async fn dispatch_stored(
        &self,
        hostname: &str,
        params: Value,
        audit_event: &str,
        audit_target: &str,
        details: Value,
    ) -> Response {
        let outcome = match self.try_send(hostname, params, None, false).await {
            Ok(outcome) => outcome,
            Err(refusal) => return self.done_stored(self.error(&refusal.sentence())),
        };
        if matches!(outcome.status, "sent" | "saved") {
            // The save itself succeeded; the device's answer is not part of it.
            let mut details = details;
            if let Some(map) = details.as_object_mut() {
                map.insert("status".into(), Value::String("sent".into()));
            }
            super::audit_action(
                self.state,
                self.user,
                audit_event,
                audit_target,
                details,
                true,
            )
            .await;
            return self.done(CommandOutcome {
                status: "saved",
                ..outcome
            });
        }
        self.audit(audit_event, audit_target, details, &outcome)
            .await;
        self.done_stored(outcome)
    }

    async fn try_execute(
        &self,
        hostname: &str,
        params: Value,
        reason: Option<String>,
    ) -> Result<CommandOutcome, Refusal> {
        self.try_send(hostname, params, reason, true).await
    }

    /// `wait`: [`CommandRelay::execute`] (the device's answer) or
    /// [`CommandRelay::dispatch`] (on the wire).
    async fn try_send(
        &self,
        hostname: &str,
        params: Value,
        reason: Option<String>,
        wait: bool,
    ) -> Result<CommandOutcome, Refusal> {
        let target = crate::relay::command_target(hostname, &params);
        let label = target_label(self.state, &target).await;
        if let Ok(mut slot) = self.target.lock() {
            *slot = Some(label);
        }
        let permissions = perms::permissions_for(self.user);

        if let Some(spec) = fiber_protocol::lookup(self.command) {
            if !permissions.iter().any(|p| p == spec.permission) {
                return Err(Refusal::Permission(spec.permission));
            }
        }

        let key = match self
            .state
            .keys
            .ensure_user_key(self.user, hostname, &permissions)
            .await
        {
            Ok(key) => key,
            Err(err) => {
                tracing::error!(%err, hostname, command = self.command, "failed to mint signing key");
                return Err(Refusal::Key(err));
            }
        };

        // Config sync (decision #43): the Portal stored the value before this call; the
        // Hub's generation moves and the Hub is hinted to pull.
        let noted = crate::sync::note_save(
            &self.state.db,
            &self.state.relay,
            hostname,
            self.command,
            &params,
            &self.user.username,
        )
        .await;

        // Cut-over (config_sync.md amendment 2026-10-15 B): a verb the document carries is
        // delivered by the pull alone. The save answers "saved"; the Hub's ack of the
        // generation settles it on the SSE status as "delivered" or "stored".
        if crate::sync::in_document(self.command) {
            let request_id = uuid::Uuid::new_v4().to_string();
            self.state.relay.note_target(&request_id, &target);
            if let Some((gen, Some(key))) = &noted {
                self.state
                    .relay
                    .track_sync_save(hostname, key, &request_id, self.command, *gen);
            }
            return Ok(CommandOutcome {
                request_id: Some(request_id),
                command_type: self.command.to_string(),
                status: "saved",
                message: None,
                error_code: None,
                result: None,
            });
        }

        let relay = &self.state.relay;
        let user = &self.user.username;
        Ok(match wait {
            true => {
                relay
                    .execute(user, &key, hostname, self.command, params, reason)
                    .await
            }
            false => {
                relay
                    .dispatch(user, &key, hostname, self.command, params, reason)
                    .await
            }
        })
    }

    /// Writes the audit row for `outcome`: `details` plus the outcome's
    /// `status`, `success` per the module doc. Nothing that could be a
    /// secret belongs in `details` — the caller's half of that promise.
    pub(crate) async fn audit(
        &self,
        event: &str,
        target: &str,
        mut details: Value,
        outcome: &CommandOutcome,
    ) {
        if let Some(map) = details.as_object_mut() {
            map.insert("status".into(), Value::String(outcome.status.to_string()));
        }
        super::audit_action(
            self.state,
            self.user,
            event,
            target,
            details,
            succeeded(outcome),
        )
        .await;
    }

    /// [`Self::execute`], then [`Self::audit`] on the real outcome, then
    /// [`Self::done`]. A refusal from `execute` is rendered and **not**
    /// audited: nothing happened, and the audit trail records actions.
    pub(crate) async fn relay(
        &self,
        hostname: &str,
        params: Value,
        reason: Option<String>,
        audit_event: &str,
        audit_target: &str,
        details: Value,
    ) -> Response {
        match self.execute(hostname, params, reason).await {
            Ok(outcome) => {
                self.audit(audit_event, audit_target, details, &outcome)
                    .await;
                self.done(outcome)
            }
            Err(refused) => self.done(refused),
        }
    }

    /// The in-process `control.radio` counterpart to [`Self::relay`]: render the
    /// seam's result and audit it on success.
    ///
    /// No signature is minted — the privilege boundary is the handler's role
    /// gate plus this audit row (plan/portal/p2p_integration.md §2.1: "signing
    /// and verifying inside one process would prove nothing"). A seam failure is
    /// audited too, as failed (LE-13): the attempt reached the seam, so it was
    /// an action.
    ///
    /// The seam's own error text is surfaced verbatim as the technical
    /// detail, the way `POST /nodes` surfaces the device's refusal string: it
    /// is the most specific thing we can say, and (unlike a relayed command's
    /// params) it never carries a secret. [`RadioError::code`] adds the
    /// sentence an operator can act on above it.
    pub(crate) async fn radio(
        &self,
        result: Result<Value, RadioError>,
        ok_message: &str,
        audit_event: &str,
        audit_target: &str,
        details: Value,
    ) -> Response {
        match result {
            Ok(_accepted) => {
                let outcome = CommandOutcome {
                    request_id: None,
                    command_type: self.command.to_string(),
                    status: "success",
                    message: Some(ok_message.to_string()),
                    error_code: None,
                    result: None,
                };
                self.audit(audit_event, audit_target, details, &outcome)
                    .await;
                self.done(outcome)
            }
            // LE-13 (local E2E 2026-09-26): an attempt the radio seam failed left no trace in
            // the audit. It reached the seam, so it was an action; it is recorded as failed,
            // with the seam's sentence.
            Err(e) => {
                let outcome = CommandOutcome {
                    error_code: e.code(),
                    ..self.error(&e.to_string())
                };
                let mut details = details;
                if let Some(map) = details.as_object_mut() {
                    map.insert("error".into(), Value::String(e.to_string()));
                }
                self.audit(audit_event, audit_target, details, &outcome)
                    .await;
                self.done(outcome)
            }
        }
    }
}

/// The verbs whose value the Portal stores before the device gets it (decision #41) —
/// since the config sync cut-over every verb the document carries
/// ([`crate::sync::in_document`]): the device's copy is secondary, so a failure to deliver
/// it is a warning ("stored"), and in-flight steps are not shown over the Portal's "saved".
pub(crate) fn stored_first(command: &str) -> bool {
    crate::sync::in_document(command)
}

/// The audit trail's `success` for a command — see the module doc. `"saved"` is a
/// document verb stored in the Portal (the Hub pulls it).
pub(crate) fn succeeded(outcome: &CommandOutcome) -> bool {
    matches!(outcome.status, "success" | "saved")
}

/// The name of a command's device without a database read: a Beacon's or Node's name as
/// the store holds it (the Portal's own name overlays the Hub's, decision #41), else a
/// Node's serial, else the id itself; a Hub's reported `device_label`. `None` for an id the
/// store does not know.
pub(crate) fn target_label_sync(state: &AppState, id: &str) -> Option<String> {
    let snapshot = state.store.snapshot();
    if let Some((_, beacon)) = snapshot.hub_of_beacon(id) {
        return Some(beacon.name.clone().unwrap_or_else(|| beacon.mac.clone()));
    }
    if let Some((_, node)) = snapshot.hub_of_node(id) {
        return Some(
            node.name
                .clone()
                .or_else(|| node.serial_number.clone())
                .unwrap_or_else(|| id.to_string()),
        );
    }
    snapshot.device(id).map(|device| {
        render::hub_display_name(Some(device), None).unwrap_or_else(|| device.hostname.clone())
    })
}

/// The name the command banner leads with (GitLab #121): the device's Portal name, falling
/// back to its serial and then its MAC / DevEUI; for a Hub the operator's name, then its
/// `device_label`, then its hostname. An id the Portal does not know reads as itself.
pub(crate) async fn target_label(state: &AppState, id: &str) -> String {
    let known_hub = state.store.display_hostname(id);
    if let Some(hostname) = known_hub {
        if let Some(name) = super::hub_routes::hub_meta(state, &hostname)
            .await
            .and_then(|m| m.name)
        {
            return name;
        }
    }
    target_label_sync(state, id).unwrap_or_else(|| id.to_string())
}
