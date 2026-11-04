//! Outbound notifications (M6-N): email (SMTP + SendGrid) and webhook
//! transports with v1-parity payloads, plus the system-alert checkers and
//! the secret-blob sealer. Pure library layer — nothing here is wired into
//! persist/routes yet (that's a follow-up package); everything is callable
//! with plain data in / data out.
//!
//! Normative spec: `plan/v1/viewer/ws.md` §"Outbound notifications".
//!
//! # v2 phase-1 deltas vs v1 (deliberate, documented)
//!
//! - **No group recipient tier.** v1 resolved recipients in two independent
//!   tiers (sensor-group config + per-device config, both could fire).
//!   Groups are out of scope for v2 phase 1, so recipient resolution is
//!   **per-device config, else the global default alarm recipients**
//!   (decisions.md #31), plus the single global system-alert list.
//! - **Webhook events are v1-parity by default**: `alarm.activated` and
//!   `alarm.test`. `alarm.escalated`, `alarm.cleared` and
//!   `alarm.acknowledged` are sent only with the opt-in `webhook_lifecycle`
//!   switch (V18, decisions.md #26 and #29): a v1 receiver that ignores
//!   `event` would read any of them as a new alarm. No signature header.
//! - **No inline logo CID.** v1 embedded a PNG logo (`proximos-logo`) in
//!   every email; v2 emails are self-contained HTML without attachments
//!   (simplification — one less asset to ship and MIME part to build).
//! - **No SSRF guard on the webhook URL** (v1 parity — v1's stdlib urllib
//!   sender had none either; the URL is operator-configured).
//! - **System-alert HTML escapes `hostname`/`message`** — fixes v1 quirk
//!   #8 (its `_system_alert_email_html` interpolated MQTT-sourced strings
//!   unescaped, unlike the alarm templates).
//! - **Viewer's own partition threshold is fixed at 70%** (v1 quirk #5
//!   parity: no column/API ever provided `viewer_storage_threshold`).

#![allow(dead_code)] // library layer until the wiring package lands (same as relay/backfill)
#![allow(unused_imports)] // ditto for the re-exports below

/// The sender name V5 seeds `notify_global_config.from_name` with. Migrations are SQL and
/// append-only, so they cannot follow the build's brand.
pub const SEEDED_FROM_NAME: &str = "ProXimos Monitor";

/// The sender name to show and send with: the stored one, except that the untouched seed
/// becomes the build's brand (`brand.toml` `monitor`, plan/portal/branding.md §3). With the
/// default brand that is the seed itself, so nothing changes there.
pub fn from_name(stored: &str) -> &str {
    if stored == SEEDED_FROM_NAME {
        proximos_brand::MONITOR
    } else {
        stored
    }
}

pub mod checkers;
pub mod email;
pub mod offline;
pub mod payload;
pub mod seal;
pub mod webhook;

pub use checkers::{
    HubFaults, SystemAlertConfig, SystemAlertEvent, SystemAlertKind, SystemAlertState,
    SystemSnapshot,
};
pub use email::{EmailTransport, SendOutcome};
pub use offline::{
    beacon_liveness, node_liveness, DeviceLiveness, DeviceScope, OfflineDetector,
    OfflineTransition, Transition,
};
pub use payload::{AckInfo, AlarmNotification, AlarmSource, EmailBody, NotifyGates, NotifyKind};
pub use seal::{NotifySealer, SealError};
pub use webhook::{send_webhook, WebhookConfig, WebhookOutcome};
