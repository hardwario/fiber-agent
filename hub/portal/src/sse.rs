//! `GET /events` — one SSE connection per browser tab (design.md §4
//! "Realtime"). Subscribes to [`StateStore`]'s `ChangeEvent` broadcast,
//! re-renders the affected partial through [`crate::render`], and pushes it
//! as a named SSE event whose name matches the partial's `sse-swap` id.
//!
//! Event naming (must match the `id`/`sse-swap` attributes the templates
//! declare):
//!
//! | `ChangeEvent`                                                | SSE event name             | partial rendered            |
//! |---------------------------------------------------------------|-----------------------------|------------------------------|
//! | `SensorLine { hostname, line }`                                | `sensor-{hostname}-{line}`  | *(id-only nudge; the Hub Sensors row refetches itself)* |
//! | `DeviceStatus`/`System`/`Power`/`Network`/`Storage`/`Standby`/`Config` | `device-{hostname}-status`  | `partials/device_status.html` |
//! | `MqttConnection`                                               | `mqtt-status`               | `partials/mqtt_banner.html` |
//! | any `ChangeEvent` or alarm nudge, at most once per `STATS_WINDOW` | `page-stats`            | *(id-only nudge; the metric row refetches `GET /stats/{scope}`, built once for all the tabs one nudge reaches)* |
//! | `Alarm`                                                        | *(ignored — see below)*     | —                            |
//! | broadcast lag (`RecvError::Lagged`)                            | `refresh` (empty data)      | — (client does a full reload) |
//!
//! `Alarm` is ignored in M1: the store always emits a `SensorLine` event
//! for the affected line alongside every `Alarm` event
//! (`state::store::apply_locked`'s `AlarmEvent` arm), and that's the only
//! fragment that renders alarm state, so re-rendering on `Alarm` too would
//! just be a redundant duplicate send.
//!
//! The `DeviceStatus`-group events all funnel into the same
//! `device-{hostname}-status` fragment, so bursts of them (e.g. a
//! `system/info` message fans out to `System`+`Power`+`Network`+`Storage`
//! in one `StateStore::apply` call, see `store.rs`) are coalesced: a simple
//! `tokio::time`-based debounce holds each hostname for
//! [`DEBOUNCE_WINDOW`] after the first event before rendering the *current*
//! state once, rather than sending one frame per event.
//!
//! Phase 1 is single-device (see `AppState` doc-comment in `http.rs`): only
//! events for the currently-displayed device (or broker-wide
//! `MqttConnection` events) are forwarded — an event for some other
//! hostname has no matching element in the DOM to swap into anyway.
//!
//! M4-C addition: alongside `StateStore`'s `ChangeEvent` broadcast, this
//! handler also subscribes [`crate::relay::CommandRelay`]'s
//! [`CommandStatus`](crate::relay::CommandStatus) broadcast and renders
//! every transition as `partials/command_status.html`, SSE event
//! `command-status` (that template's own doc comment is the binding
//! contract for the `cmd` context shape). `CommandStatus` carries no
//! hostname (mqtt.md's signed-command handshake is per-request, not
//! per-device-page), so — unlike the `ChangeEvent` table above — every
//! transition is forwarded regardless of which device is on screen; the
//! partial's own doc-comment notes there is exactly one `#command-status`
//! element per page, so this can't collide with anything else in the DOM.
//!
//! M5-F addition: a third subscription, [`crate::backfill::BackfillService`]'s
//! [`BackfillNudge`] broadcast. Every nudge (job created/cancelled/status
//! change/refusal, plus each history-progress frame) re-fetches that Node's
//! jobs and re-renders `partials/backfill_jobs.html` as SSE event
//! `backfill-jobs-{node_key}` (the partial's `sse-swap` id). Like
//! `command-status`, nudges are forwarded regardless of the device on
//! screen — a tab without the matching `#backfill-jobs-{node_key}` element
//! simply has nowhere to swap the fragment. `can_edit` reuses the
//! connection's captured role (M4-D) to gate the Cancel button.
//!
//! M5-G addition: the Node (STICKER) / Beacon (EYE) `ChangeEvent` arms this
//! module used to no-op:
//!
//! | `ChangeEvent`                     | SSE event name                        | partial rendered              |
//! |------------------------------------|----------------------------------------|--------------------------------|
//! | `NodeBatch { hostname }`           | `node-{hostname}-{node_key}` (one per Node) | `partials/node_card.html` |
//! | `Node { hostname, node_key }`       | `node-{hostname}-{node_key}`            | `partials/node_card.html`     |
//! | `NodeAdded { hostname, node_key }` | `node-added-{hostname}`                | `partials/node_card.html` (prepended into `#node-rows`) |
//! | `NodeRemoved { hostname, node_key }` | `node-removed-{hostname}`            | a stub `<tr hx-swap-oob="delete">` |
//! | `NodeConfig { hostname, node_key }` | `pending-config-{node_key}`, `node-device-{node_key}` **and** `node-config-{node_key}` | `partials/pending_config_body.html`, `partials/node_device_body.html`, `partials/node_config_body.html` |
//! | `NodeCommandResult { .. }`         | `command-status`                       | `partials/command_status.html` |
//! | `BeaconBatch { hostname }`         | `beacon-{hostname}-{mac\|replace(':','-')}` (one per REGISTERED beacon) | `partials/beacon_card.html` |
//! | `Beacon { hostname, mac }`         | same, registered beacons only          | `partials/beacon_card.html`   |
//! | `BeaconBatch { hostname }`         | `beacons-discovered-{hostname}` (once)  | `partials/discovered_table.html` (B6) |
//! | `BeaconAdded { .. }`               | `refresh` (empty data)                 | — (a Beacon enrolled: no card to swap into yet) |
//! | `BeaconRemoved { discovered: false, .. }` | `refresh` (empty data)          | — (an enrolled Beacon deregistered) |
//! | `BeaconRemoved { discovered: true, .. }`  | — (nothing)                     | — (its `BeaconBatch` repaints the scan panel) |
//!
//! `NodeBatch`/`BeaconBatch` fan out to one event per Node/beacon rather
//! than a single combined frame (work package: "batch events → re-render
//! every card of that hostname as individual events") — cards are
//! independently id'd, so there is no shared fragment to send instead.
//!
//! Discovered-only beacon candidates (`discovered == Some(true)`) get **no
//! per-card frame**: they have no `beacon-{hostname}-{mac}` target, because
//! they are not rows on `/beacons` at all. Since B6 they have a target of
//! their own — the whole scan panel, `beacons-discovered-{hostname}`,
//! rendered once per `BeaconBatch` into the shell on `/beacons/add`
//! (`partials/discovered_table.html`). One frame for the table rather than
//! one per stranger, because a candidate appearing or vanishing is a
//! membership change and there is no row to swap.
//!
//! Before B6 a candidate's fields went stale until the next full
//! navigation and only its *removal* sent the `refresh` hint. That hint is
//! now **split by which half was removed**, because the two are not equally
//! rare and do not cost the same:
//!
//! * A **candidate** pruned by reconcile-by-absence sends nothing at all.
//!   It has no card outside the scan panel, and the `BeaconBatch` that
//!   comes with the same snapshot repaints that panel whole. Under
//!   reconcile-by-absence a stranger walking out of range is an ordinary
//!   event on a busy site — not the infrequent one this note used to claim
//!   — and a page-level reload would wipe the registration form the
//!   operator is typing into directly above the table (B6 review, item 2;
//!   `pages/beacon_add.html` carries no `data-sse-refresh` for the same
//!   reason).
//! * An **enrolled** Beacon deregistered from `config/state.eye_tags` still
//!   sends `refresh`: it *is* a row on `/beacons` and on the group pages,
//!   and no fragment can express a card that should stop existing. That one
//!   really is infrequent — an operator removing a device, not a radio going
//!   quiet.
//!
//! `NodeConfig` (covering the `node_configs`/`node_infos`/
//! `node_full_configs` caches, including multi-chunk full-config reads) was
//! a documented no-op through M5-G and B3, for a reason that was true at
//! the time: `pages/node_detail.html` defined no `id`/`sse-swap` for its
//! config section, and mapping the event to `refresh` would have reloaded
//! every open tab — including the `/nodes` list, which has no use for one
//! Node's config chunk — once per config page.
//!
//! **B4 gave it two targets and it renders both.** The config card moved
//! into `partials/node_config.html` so it could carry
//! `node-config-{node_key}`, and §3.5's unsent-configuration banner
//! (`pending-config-{node_key}`) repaints alongside it: a config page
//! arriving from the device is exactly what turns the banner's "on the
//! device" column from a guess into a fact, and it is the moment after a
//! Send when an operator most wants to see it. The info tombstone rides
//! this event too and clears a cache the config table does not read — that
//! path repaints the banner and stops.
//!
//! B4 also added two arms of its own: `NodeAdded` prepends a row into the
//! `/nodes` table (`node-added-{hostname}`) and `NodeRemoved` deletes one
//! out of it (`node-removed-{hostname}`, an `hx-swap-oob="delete"` stub),
//! replacing the page-level `refresh` that arm used to send.
//!
//! MB2 addition — the monitoring page's live rows. Alongside whatever
//! fragment an arm below renders, `handle_change` first emits an **id-only**
//! nudge for every monitoring row the event can move, derived by
//! [`crate::monitoring::names_for_device`]:
//!
//! | `ChangeEvent`                | SSE event name                          | data     |
//! |------------------------------|-----------------------------------------|----------|
//! | `SensorLine { hostname, line }` | `mon-probe-{hostname}-{line}`        | *(empty)* |
//! | `DeviceStatus { hostname }`  | `mon-probe-…`, `mon-node-…`, `mon-beacon-…` (every row of that Hub; UX-053, #30c) | *(empty)* |
//! | `Node` / `NodeBatch`         | `mon-node-{hostname}-{node_key}` (one per Node) | *(empty)* |
//! | `Beacon` / `BeaconBatch`     | `mon-beacon-{hostname}-{mac\|replace(':','-')}` (one per beacon) | *(empty)* |
//! | `Groups`                     | `refresh` (empty data)                  | — (full reload) |
//!
//! The "Radio & gateways" card (devices.hub.md §6, §12 A/A2) is nudged the same way:
//! `Radio { hostname }` sends `radio-{hostname}`, and the cluster-wide `RadioGateways`
//! sends `radio-gateways`. Both are id-only — the card's fragment lands with the step that
//! renders it.
//!
//! **The frame carries no markup on purpose.** A broadcast fragment is one
//! payload for every viewer, while `/monitoring` renders in one of four
//! layouts chosen *per operator* — so the name says only which entity moved,
//! and each browser refetches
//! `GET /monitoring/groups/{id}/rows/{token}` (and the group's
//! `…/header`, whose counts are aggregates over the rows), which renders in
//! *that* session's view mode. That inversion is why MB2 adds no per-row
//! `ChangeEvent` variant: every event that already fires on a reading
//! carries exactly the identity a nudge needs.
//!
//! Every arm renders for whichever Hub the event names. Until devices.hub.md
//! §12 E (2026-09-15) each arm dropped events for any Hub but the first in
//! the store, because the device pages showed only that one; with
//! `/hubs/{hub}/…` every known Hub has a page, and the event names were
//! already hostname-qualified, so the gate went and nothing else changed.
//!
//! A fault appearing or clearing needs no separate wiring either:
//! `OfflineService::announce` already repaints by emitting
//! `ChangeEvent::Node`/`Beacon` on every liveness transition, so a sweep
//! that finds a Node silent produces the same `mon-node-…` nudge without an
//! MQTT message having arrived (held by `a_liveness_transition_nudges_the_monitoring_row`).
//!
//! `NodeCommandResult` is **not** relay's `CommandStatus` stream in
//! disguise. The Node verbs this event covers —
//! `get_sticker_config`/`get_sticker_full_config`/`get_sticker_history`/
//! `sticker_force_send` — ride the one signed dialect since the MQTT
//! rebuild (`Routing::Plain` and `execute_plain` are gone), so the relay
//! *does* now learn the device's `config/response` and emits
//! `sent → success`/`error` for them. What it still cannot see is the
//! **Node's** answer: the Hub's `config/response` says the Hub accepted the
//! downlink, and the Node's own `result`/`detail` arrives later, on the raw
//! `…/{dev_eui}/command` passthrough (ws.md: "never cached"), as this
//! `ChangeEvent`. Rendering it through `partials/command_status.html`
//! therefore adds information relay's own stream doesn't carry: a further
//! `command-status` frame with the Node's real result, distinguishable
//! from the relay's frames by its `target`, the Node it names beside the
//! command's translated name (the raw payload carries no
//! `request_id` to correlate against — `cmd.request_id` renders empty,
//! same harmless fallback `CommandOutcomeCtx` already uses). An operator
//! may thus see the Hub's `success` and then the Node's result for one
//! command; they are two facts about two devices, not a double report.

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use minijinja::value::Value;
use minijinja::Environment;
use portal_storage::AlarmListFilter;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use crate::alarms::{AlarmNudge, AlarmService};
use crate::auth::CurrentUser;
use crate::backfill::{self, BackfillNudge, BackfillService};
use crate::http::AppState;
use crate::i18n;
use crate::relay::CommandStatus;
use crate::render;
use crate::routes::beacon_routes::beacon_row_data;
use crate::routes::node_routes::node_row_data;
use crate::state::{ChangeEvent, StateStore};

/// Known-good `…/{dev_eui}/command` result strings across the Node Plain
/// commands `NodeCommandResult` covers (`get_sticker_config`,
/// `get_sticker_full_config`, `get_sticker_history`, `sticker_force_send`) —
/// same list `backfill.rs`'s private `OK_RESULTS` uses for
/// `get_sticker_history` specifically, widened here since this handler
/// renders every Node command's result, not just history requests.
pub(crate) const NODE_COMMAND_OK_RESULTS: [&str; 4] = ["ok", "requested", "accepted", "success"];

/// The payload every id-only frame carries.
///
/// **One byte, not zero.** An SSE frame whose data buffer is empty is not
/// dispatched at all — the HTML standard's event-stream parser returns
/// without firing when the buffer is empty ("If the data buffer is an empty
/// string, set the data buffer and the event type buffer to the empty string
/// and return"), so `data("")` produces a frame that reaches the browser and
/// is then silently discarded. That is not a subtlety anything but a real
/// browser will show: the frame is on the wire, `curl` prints it, and
/// nothing happens. It cost MB2 an afternoon and it had also been quietly
/// disabling every `refresh` hint on this connection since M1-H.
///
/// The value itself means nothing; the frame's *name* is the whole message.
const NUDGE_PAYLOAD: &str = "1";

/// The one thing about [`NUDGE_PAYLOAD`] that must never be true again, and
/// the cheapest possible place to hold it: an empty payload compiles, ships,
/// reaches the browser and does nothing (monitoring.backend.md §15.2 — it
/// cost MB2 an afternoon and it had silently disabled every `refresh` hint
/// since M1-H). A test would only catch it when run; this fails the build.
const _: () = assert!(
    !NUDGE_PAYLOAD.is_empty(),
    "an empty-data SSE frame is never dispatched — monitoring.backend.md §15.2"
);

/// How long a hostname's device-status fragment waits for more of the same
/// before it's actually rendered and sent — see module doc.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(250);

/// How long the `page-stats` nudge waits after the first change that may
/// move the metric row (UX-020). Armed once and not re-armed by later
/// changes, so steady telemetry yields one recount per window per tab
/// rather than starving it (monitoring.md §7, devices.node.md §2).
const STATS_WINDOW: Duration = Duration::from_secs(2);

/// How often the debounce loop checks for expired windows. Coarser than
/// [`DEBOUNCE_WINDOW`] would just add latency to the flush; finer buys
/// nothing since nothing can flush sooner than the window itself.
const DEBOUNCE_TICK: Duration = Duration::from_millis(50);

/// SSE event names are built from `hostname`, `node_key` and MAC — all of
/// them MQTT topic segments, which `parse_node_topic` only checks for
/// non-emptiness. `Event::event` **panics** on a newline or carriage return,
/// and that panic would land in the per-connection task, so a single crafted
/// topic could drop every open operator's stream. Stripped rather than
/// rejected: the name is a routing label for htmx, and a device whose id
/// carries a stray control character should still be visible in the UI.
fn sse_event_name(name: impl Into<String>) -> String {
    let name = name.into();
    if name.contains(['\n', '\r']) {
        return name.replace(['\n', '\r'], "");
    }
    name
}

/// Outbound channel capacity between the per-connection render task and the
/// SSE response stream. Generous relative to the 8-sensor-line dashboard;
/// a full channel just means the render task's `send` awaits briefly.
const OUT_CHANNEL_CAPACITY: usize = 64;

/// M2-C: auth-gated like every other page — an anonymous connection attempt
/// never reaches the body below (the `CurrentUser` extractor itself 303s to
/// `/login`, and htmx's `sse-connect` sends the session cookie automatically
/// like any other same-origin request).
///
/// M4-D: `can_edit` (the connected user's role, `render::can_edit_thresholds`)
/// is captured once per connection and threaded into every sensor-card /
/// device-status re-render this connection ever sends — otherwise a live
/// SSE update would reset a manager/admin's threshold-edit pencil to
/// hidden the moment anything on the card changed.
///
/// M9: `lang` is captured the same way, for the same reason `can_edit` is —
/// `crate::i18n::current_lang()` reads the per-request task-local set by
/// `i18n::lang_middleware`, which is only valid up to this point; the
/// `tokio::spawn` below starts a new task that does not inherit it (see
/// `i18n.rs`'s `CURRENT_LANG` doc comment), so the resolved value has to be
/// carried explicitly from here on, like `can_edit` already is.
pub async fn events_handler(
    current_user: CurrentUser,
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let can_edit = render::can_edit_thresholds(&current_user);
    let lang = i18n::current_lang();
    let (tx, rx) = mpsc::channel(OUT_CHANNEL_CAPACITY);
    tokio::spawn(run(state, can_edit, lang, tx));

    let stream = ReceiverStream::new(rx).map(Ok);
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Drives one SSE connection until the client disconnects (mpsc `send`
/// starts failing once the receiver — owned by the response stream — is
/// dropped).
#[allow(clippy::too_many_arguments)]
/// Takes the whole [`AppState`] rather than the seven pieces of it this
/// loop happens to use. It had grown to seven positional arguments of which
/// six were `state.<field>` at every call site, and B1 needed two more
/// (`offline`, `signal_quality`) — at which point the parameter list is just
/// a hand-maintained copy of the struct.
async fn run(state: AppState, can_edit: bool, lang: &'static str, tx: mpsc::Sender<Event>) {
    // B4's banner needs the whole state (the draft, the merged thresholds
    // and the device's config pages come from three different handles), so
    // one clone is kept alongside the destructured pieces rather than
    // adding three more fields to the pattern below.
    let state_for_pending = state.clone();
    let AppState {
        mut shutdown,
        store,
        relay,
        backfill,
        beacon_history,
        alarms,
        pending,
        templates,
        ..
    } = state;

    let mut changes = store.subscribe();
    let mut cmd_status = relay.subscribe_status();
    let mut backfill_nudges = backfill.subscribe_nudges();
    let mut beacon_history_nudges = beacon_history.subscribe_nudges();
    let mut alarm_nudges = alarms.subscribe_nudges();
    let mut pending_nudges = pending.subscribe_nudges();
    // hostname -> when its coalesced device-status fragment should render.
    let mut debounce: HashMap<String, Instant> = HashMap::new();
    // When this tab's metric row gets its `page-stats` nudge. Every store
    // change arms it: a reading, a liveness flip or a Hub status can each move
    // a tile, and telling them apart would cost more than the nudge.
    let mut stats_due: Option<Instant> = None;
    // Collapses the `NodeAdded` + `Node`/`NodeBatch` pair a registration
    // emits back to back into one bulk load (§21 review, item 7).
    let mut memos = RowDataMemos::default();
    let mut ticker = tokio::time::interval(DEBOUNCE_TICK);

    loop {
        tokio::select! {
            // Shutdown ends the subscription instead of outliving the server:
            // dropping `tx` finishes the response stream, which is what lets
            // axum's graceful shutdown actually complete. `changed()` errors
            // only if the sender is gone, which also means we are shutting
            // down, so both arms exit.
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    tracing::debug!("SSE stream closing: shutdown signalled");
                    return;
                }
            }
            received = changes.recv() => {
                match received {
                    Ok(change) => {
                        stats_due.get_or_insert_with(|| Instant::now() + STATS_WINDOW);
                        if !handle_change(change, can_edit, lang, &tx, &mut debounce, &mut memos, &state_for_pending).await {
                            return;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "SSE subscriber lagged; sending full-refresh hint");
                        if tx.send(Event::default().event("refresh").data(NUDGE_PAYLOAD)).await.is_err() {
                            return;
                        }
                    }
                    Err(RecvError::Closed) => return,
                }
            }
            received = cmd_status.recv() => {
                match received {
                    Ok(status) => {
                        // GitLab #121: the banner leads with the device's name.
                        let target = match status.target.as_deref() {
                            Some(id) => Some(crate::routes::command::target_label(&state_for_pending, id).await),
                            None => None,
                        };
                        if !handle_command_status(status, target, &templates.env(), lang, &tx).await {
                            return;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        // Unlike the ChangeEvent lag path, there's no
                        // fragment to full-refresh here beyond the single
                        // command-status alert, and a stale in-flight
                        // status is self-correcting on the next transition
                        // — log and keep going rather than force a reload.
                        tracing::warn!(missed = n, "command-status subscriber lagged");
                    }
                    Err(RecvError::Closed) => return,
                }
            }
            received = backfill_nudges.recv() => {
                match received {
                    Ok(nudge) => {
                        if !handle_backfill_nudge(nudge, &backfill, &store, &templates.env(), can_edit, lang, &tx).await {
                            return;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        // A missed nudge leaves one jobs table stale until
                        // its next lifecycle edge — self-correcting (the
                        // reconciler alone guarantees more edges for any
                        // non-terminal job), so log and keep going, same
                        // as the command-status lag path.
                        tracing::warn!(missed = n, "backfill-nudge subscriber lagged");
                    }
                    Err(RecvError::Closed) => return,
                }
            }
            received = beacon_history_nudges.recv() => {
                match received {
                    Ok(nudge) => {
                        if !handle_beacon_history_nudge(nudge, &beacon_history, &store, &templates.env(), can_edit, lang, &tx).await {
                            return;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        // Same reasoning as the backfill-nudge lag path.
                        tracing::warn!(missed = n, "beacon-history-nudge subscriber lagged");
                    }
                    Err(RecvError::Closed) => return,
                }
            }
            received = pending_nudges.recv() => {
                match received {
                    Ok(nudge) => {
                        if !handle_pending_nudge(nudge, &state_for_pending, can_edit, lang, &tx).await {
                            return;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        // Same reasoning as the backfill-nudge lag path: the
                        // banner stays one edit stale until the next nudge,
                        // and a send walk emits several per unit.
                        tracing::warn!(missed = n, "pending-config-nudge subscriber lagged");
                    }
                    Err(RecvError::Closed) => return,
                }
            }
            received = alarm_nudges.recv() => {
                match received {
                    Ok(nudge) => {
                        // Hub faults (the Faults tile) move with the alarm engine.
                        stats_due.get_or_insert_with(|| Instant::now() + STATS_WINDOW);
                        if !handle_alarm_nudge(nudge, &alarms, &store, &templates.env(), lang, &tx).await {
                            return;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        // Same reasoning as the backfill-nudge lag path: the
                        // table just stays one mutation stale until the next
                        // nudge, which the alarm engine guarantees will come
                        // for any alarm that's still active.
                        tracing::warn!(missed = n, "alarm-nudge subscriber lagged");
                    }
                    Err(RecvError::Closed) => return,
                }
            }
            _ = ticker.tick() => {
                // Id-only: the row refetches `GET /stats/{scope}` for the
                // page it sits on, which only the browser knows.
                if stats_due.is_some_and(|due| due <= Instant::now()) {
                    stats_due = None;
                    if tx.send(Event::default().event("page-stats").data(NUDGE_PAYLOAD)).await.is_err() {
                        return;
                    }
                }
                if !flush_due(&store, &templates.env(), can_edit, lang, &tx, &mut debounce).await {
                    return;
                }
            }
        }
    }
}

/// Re-renders the alarm table (`partials/alarm_table.html`, ctx contract
/// `{alarms, total, limit, offset, filter}`) as SSE event `alarm-table` on
/// every `AlarmNudge`, using a fixed "active-first" default filter
/// (`include_acknowledged: true, include_cleared: false, limit: 50`) —
/// M6-E's work order default, independent of whatever filter the connected
/// tab's `/alarms` page last loaded with (phase-1 has no per-connection
/// filter state to restore here, same simplification `partials/
/// alarm_table.html`'s SSE-swap already assumes). A fetch failure drops the
/// frame (the next nudge retries); returns `false` once the client has
/// disconnected.
///
/// Additionally renders one pop-up (`partials/alarm_toast.html`, SSE event
/// `alarm-toast`, ctx contract `{a, escalated}` — `a` = `render::AlarmCtx`)
/// per id in `nudge.activated_ids` (rows the mutation *opened*) and
/// `nudge.escalated_ids` (rows it *escalated to critical*, rendered with
/// the escalation label; de-escalations/acks/clears/state refreshes stay
/// silent — see `AlarmNudge`). The row is re-read here rather than carried
/// in the nudge (the nudge channel's coarse-signal philosophy); a row that
/// vanished or fails to load just drops its toast — the alarm table above
/// already shows it.
async fn handle_alarm_nudge(
    nudge: AlarmNudge,
    alarms: &AlarmService,
    store: &crate::state::StateStore,
    templates: &Environment<'static>,
    lang: &'static str,
    tx: &mpsc::Sender<Event>,
) -> bool {
    tracing::debug!(hostname = %nudge.hostname, "alarm nudge received; re-rendering alarm table");
    let filter = AlarmListFilter {
        limit: 50,
        offset: 0,
        include_acknowledged: true,
        include_cleared: false,
        // The nudge re-renders one FIXED view; a viewer's own search belongs
        // to their query string, not to what the server broadcasts. Which is
        // also why pages/alarms.html sets `sse_live = false`.
        search: None,
        search_targets: Vec::new(),
        hostname: None,
        to_state: None,
        alarm_type: None,
        severity: None,
        // The alarm table is fleet-wide; MB3's per-probe filters belong to
        // the probe-detail window and nothing else reads them here.
        sensor_id: None,
        sensor_id_prefix: None,
        sensor_field: None,
        // The broadcast view is the list's default order. A viewer who sorted
        // by hand owns that in their query string, same as the search above.
        sort: portal_storage::AlarmSort::Start,
        sort_desc: true,
        triggered_since: None,
        triggered_until: None,
    };
    let (limit, offset) = (filter.limit, filter.offset);
    let filter_ctx = minijinja::context! {
        hostname => filter.hostname.clone(),
        severity => filter.severity.clone(),
        alarm_type => filter.alarm_type.clone(),
        include_acknowledged => filter.include_acknowledged,
        include_cleared => filter.include_cleared,
    };

    let rows = match alarms.list(filter).await {
        Ok((rows, total)) => {
            let snapshot = store.snapshot();
            let rows: Vec<render::AlarmCtx> = rows
                .iter()
                .map(|r| render::alarm_ctx(r, &snapshot))
                .collect();
            (rows, total)
        }
        Err(err) => {
            tracing::warn!(%err, "failed to list alarms for SSE alarm-table; dropping");
            return true;
        }
    };

    let ok = send_rendered(
        templates,
        tx,
        "partials/alarm_table.html",
        minijinja::context! {
            alarms => rows.0,
            total => rows.1,
            limit => limit,
            offset => offset,
            filter => filter_ctx,
            // The broadcast table is the default order, unfiltered — the
            // headers and the empty state read these back.
            sort => "start",
            dir => "desc",
            sort_qs => "",
            filtered => false,
        },
        "alarm-table".to_string(),
        lang,
    )
    .await;
    if !ok {
        return false;
    }

    let toasts = nudge
        .activated_ids
        .iter()
        .map(|id| (id, false))
        .chain(nudge.escalated_ids.iter().map(|id| (id, true)));
    for (id, escalated) in toasts {
        let row = match alarms.detail(id).await {
            Ok(Some((row, _notes))) => row,
            Ok(None) => continue,
            Err(err) => {
                tracing::warn!(%err, id, "failed to load alarm for SSE toast; dropping");
                continue;
            }
        };
        let ctx = render::alarm_ctx(&row, &store.snapshot());
        let ok = send_rendered(
            templates,
            tx,
            "partials/alarm_toast.html",
            minijinja::context! { a => ctx, escalated => escalated },
            "alarm-toast".to_string(),
            lang,
        )
        .await;
        if !ok {
            return false;
        }
    }
    true
}

/// Re-renders one Node's backfill jobs table
/// (`partials/backfill_jobs.html`, ctx contract `{node_key, can_edit,
/// jobs, gaps}`) as SSE event `backfill-jobs-{node_key}`. A fetch failure
/// drops the frame (the next nudge retries); returns `false` once the
/// client has disconnected.
///
/// The gap list (B3, §6) is recomputed here rather than carried on a
/// second event: a credited history frame both advances a job and shortens
/// the hole it is filling, so the two have to repaint together or the page
/// shows a filled gap next to a done job. `nudge.hostname` is why
/// [`BackfillNudge`] carries one.
async fn handle_backfill_nudge(
    nudge: BackfillNudge,
    backfill: &BackfillService,
    store: &StateStore,
    templates: &Environment<'static>,
    can_edit: bool,
    lang: &'static str,
    tx: &mpsc::Sender<Event>,
) -> bool {
    let jobs = match backfill.jobs_for(&nudge.node_key, None).await {
        Ok(jobs) => jobs,
        Err(err) => {
            tracing::warn!(%err, node_key = %nudge.node_key, "failed to fetch backfill jobs for SSE; dropping");
            return true;
        }
    };
    let rows: Vec<backfill::JobCtx> = jobs.iter().map(backfill::job_ctx).collect();
    let gaps = backfill
        .gaps_ctx(store, &nudge.hostname, &nudge.node_key)
        .await;
    let history_off = backfill::node_keeps_no_history(store, &nudge.hostname, &nudge.node_key);
    let node_key = nudge.node_key;
    send_rendered(
        templates,
        tx,
        "partials/backfill_jobs.html",
        minijinja::context! { node_key => node_key, can_edit => can_edit, jobs => rows, gaps => gaps, history_off => history_off },
        format!("backfill-jobs-{node_key}"),
        lang,
    )
    .await
}

/// Re-renders one Beacon's History tab table — the Node's
/// `partials/backfill_jobs.html`, addressed by `swap_id`/`post_url`
/// (plan/portal/beacon_history.md §7.1) — as SSE event
/// `beacon-history-{mac with dashes}`. Jobs and gaps repaint together for the
/// reason [`handle_backfill_nudge`] gives.
async fn handle_beacon_history_nudge(
    nudge: crate::beacon_history::BeaconHistoryNudge,
    beacon_history: &crate::beacon_history::BeaconHistoryService,
    store: &StateStore,
    templates: &Environment<'static>,
    can_edit: bool,
    lang: &'static str,
    tx: &mpsc::Sender<Event>,
) -> bool {
    let jobs = beacon_history
        .jobs_ctx(store, &nudge.hostname, &nudge.mac)
        .await;
    let gaps = beacon_history.gaps_ctx(&nudge.hostname, &nudge.mac).await;
    let swap_id = crate::beacon_history::swap_id(&nudge.mac);
    send_rendered(
        templates,
        tx,
        "partials/backfill_jobs.html",
        minijinja::context! {
            swap_id => swap_id.clone(),
            post_url => crate::beacon_history::post_url(&nudge.mac),
            can_edit => can_edit,
            jobs => jobs,
            gaps => gaps,
        },
        swap_id,
        lang,
    )
    .await
}

/// Re-renders one Node's unsent-configuration banner
/// (`partials/pending_config.html`) as `pending-config-{node_key}` on every
/// [`PendingNudge`] — B4's twin of [`handle_backfill_nudge`].
///
/// Every open tab gets it, which is the point: devices.node.md §3.5 asks
/// for a draft that is visible to every user, so a manager typing a value
/// into one browser has to appear in the colleague's other browser without
/// a reload. It is also what animates a send — the walk nudges on every
/// state change, so the row moves pending -> sending -> gone while the
/// request that started it is still open.
///
/// Takes the whole `AppState`: the banner's three inputs (the draft, the
/// merged thresholds, the device's config pages) come from three different
/// handles, and `node_routes::pending_ctx_for` is the one builder all four
/// producers share.
///
/// **The event name carries no hostname** (§21 review, item 14). The
/// hostname is used — `nudge.hostname` is what `pending_ctx_for` reads the
/// draft for — it just does not reach the SSE event id. Phase 1 is
/// single-Hub (`AppState`'s doc comment), so `pending-config-{node_key}` is unambiguous
/// and matches the id the banner declares. Multi-Hub (§14.5) would need the
/// hostname in both the event name and the template's id, and every other
/// Node event already carries it (`node-{hostname}-{node_key}`); this one
/// is the outlier to fix when that lands, not a bug today.
async fn handle_pending_nudge(
    nudge: crate::pending::PendingNudge,
    state: &AppState,
    can_edit: bool,
    lang: &'static str,
    tx: &mpsc::Sender<Event>,
) -> bool {
    let ctx = crate::routes::node_routes::pending_ctx_for(
        state,
        &nudge.hostname,
        &nudge.node_key,
        can_edit,
    )
    .await;
    let node_key = nudge.node_key;
    let templates = state.templates.env();
    if !send_rendered(
        &templates,
        tx,
        "partials/pending_config_body.html",
        minijinja::context! { pending => ctx },
        format!("pending-config-{node_key}"),
        lang,
    )
    .await
    {
        return false;
    }
    // `/limits` shows each Node limit's sync state (draft, sending, refused) and reloads on
    // this nudge; it has no fragment of its own.
    if tx
        .send(Event::default().event("limits-pending").data(NUDGE_PAYLOAD))
        .await
        .is_err()
    {
        return false;
    }
    // The config card's History switch says whether a change waits in the draft, so a draft
    // that moved repaints it too (devices.node.md §3.9, amendment 2026-09-25).
    send_config_body(
        state,
        &templates,
        tx,
        &nudge.hostname,
        &node_key,
        can_edit,
        lang,
    )
    .await
}

/// The config card's body (`node-config-{node_key}`), with its History switch. One builder for
/// both events that repaint it, so a config page and a draft change draw the same card.
async fn send_config_body(
    state: &AppState,
    templates: &minijinja::Environment<'static>,
    tx: &mpsc::Sender<Event>,
    hostname: &str,
    node_key: &str,
    can_edit: bool,
    lang: &'static str,
) -> bool {
    let config = state
        .store
        .snapshot_device(hostname)
        .and_then(|d| d.node_configs.get(node_key).cloned());
    let history = crate::routes::node_routes::history_ctx_for(state, hostname, node_key).await;
    send_rendered(
        templates,
        tx,
        "partials/node_config_body.html",
        minijinja::context! {
            n => minijinja::context! { node_key => node_key.to_string() },
            can_edit => can_edit,
            config => crate::render::node_config_ctx(config.as_ref()),
            history => history,
        },
        format!("node-config-{node_key}"),
        lang,
    )
    .await
}

/// One connection's short-lived memo of [`node_row_data`] (§21 review,
/// item 7).
///
/// A Node registration emits `NodeAdded` **and** `Node`/`NodeBatch` back to
/// back (`state/store.rs` pushes the insert first so a connection prepends
/// the row and then repaints it), and both arms load the four bulk queries
/// `node_row_data` runs. That is eight fleet-wide queries per registration
/// per open tab, for two renders of the same data microseconds apart.
///
/// The memo collapses exactly that burst and nothing wider: it is valid for
/// [`ROW_DATA_MEMO_TTL`], keyed by hostname, and per connection. The four
/// tables behind it are operator-edited (descriptions, thresholds, drafts,
/// job states) plus a reading index — none of them changes meaningfully
/// inside a quarter of a second, and anything that does gets its own event
/// and a fresh load.
struct NodeRowDataMemoEntry {
    hostname: String,
    loaded_at: Instant,
    data: render::NodeRowData,
}

/// `None` until this connection's first Node event. Held by
/// [`run`] and threaded into [`handle_change`].
type NodeRowDataMemo = Option<NodeRowDataMemoEntry>;

/// How long a memoed [`NodeRowData`](render::NodeRowData) is reused. Long
/// enough to cover one store `apply()`'s worth of events, short enough that
/// nobody has to reason about staleness.
const ROW_DATA_MEMO_TTL: Duration = Duration::from_millis(250);

/// Reads the memo or refills it.
async fn memoed_row_data<'a>(
    memo: &'a mut NodeRowDataMemo,
    state: &AppState,
    hostname: &str,
) -> &'a render::NodeRowData {
    let fresh = memo
        .as_ref()
        .is_some_and(|m| m.hostname == hostname && m.loaded_at.elapsed() < ROW_DATA_MEMO_TTL);
    if !fresh {
        let data = node_row_data(state, hostname).await;
        *memo = Some(NodeRowDataMemoEntry {
            hostname: hostname.to_string(),
            loaded_at: Instant::now(),
            data,
        });
    }
    &memo.as_ref().expect("just filled").data
}

/// The Beacon twin of [`NodeRowDataMemo`].
///
/// `ChangeEvent::Beacon` fires **per advertisement**, and its arm ran
/// `beacon_row_data` — two fleet-wide reads, `device_meta_map` and
/// `field_thresholds_by_entity` — on every one of them, per open
/// connection, to repaint a card whose RSSI moved by one. Forty beacons
/// advertising at ~1 Hz with eight tabs open is 640 SQLite reads a second
/// for operator-edited tables that change by the week.
///
/// The Node side got this treatment in the §21 review (item 7) and the
/// Beacon arms were left behind; same TTL, same per-connection scope, same
/// argument — descriptions and thresholds are operator-edited, and an edit
/// gets its own event and a fresh load.
struct BeaconRowDataMemoEntry {
    hostname: String,
    loaded_at: Instant,
    data: render::BeaconRowData,
}

type BeaconRowDataMemo = Option<BeaconRowDataMemoEntry>;

/// One connection's bulk-load memos, in one place so the arms that need
/// them take a single handle rather than growing the parameter list per
/// device kind — the shape §21 review item 16 already argued for.
#[derive(Default)]
struct RowDataMemos {
    node: NodeRowDataMemo,
    beacon: BeaconRowDataMemo,
}

/// Reads the memo or refills it.
async fn memoed_beacon_row_data<'a>(
    memo: &'a mut BeaconRowDataMemo,
    state: &AppState,
    hostname: &str,
) -> &'a render::BeaconRowData {
    let fresh = memo
        .as_ref()
        .is_some_and(|m| m.hostname == hostname && m.loaded_at.elapsed() < ROW_DATA_MEMO_TTL);
    if !fresh {
        let data = beacon_row_data(
            &state.db,
            &state.offline,
            &state.calibration,
            state.signal_quality,
            hostname,
        )
        .await;
        *memo = Some(BeaconRowDataMemoEntry {
            hostname: hostname.to_string(),
            loaded_at: Instant::now(),
            data,
        });
    }
    &memo.as_ref().expect("just filled").data
}

/// The SSE event the cluster's merged scan panel on `/beacons/add` listens on (decision #42).
pub(crate) const CLUSTER_SCAN_EVENT: &str = "beacons-discovered-cluster";

/// The hostname a cluster's Beacon rows are keyed under — the Server's, as the store spells
/// it — or `None` outside a cluster or before the Server itself has reported.
fn cluster_beacon_row_host(snapshot: &crate::state::PortalState) -> Option<String> {
    snapshot
        .beacon_registry()
        .map(|device| device.hostname.clone())
}

/// Returns `false` once the client has disconnected (send failed), so the
/// caller can stop the task instead of looping forever against a dead
/// channel.
///
/// Takes the `AppState` and nothing else it could reach through it (§21
/// review, item 16). It used to take five destructured handles *and* the
/// state, which is the same values twice: the state was added for the one
/// arm that needed three handles at once, and the parameter list then read
/// as a hand-maintained copy of a struct that was already being passed.
///
/// `memos` are per-connection memos, not caches: see [`NodeRowDataMemo`]
/// and [`BeaconRowDataMemo`].
async fn handle_change(
    change: ChangeEvent,
    can_edit: bool,
    lang: &'static str,
    tx: &mpsc::Sender<Event>,
    debounce: &mut HashMap<String, Instant>,
    memos: &mut RowDataMemos,
    state: &AppState,
) -> bool {
    let store = &state.store;
    let templates = &state.templates.env();
    let signal_quality = state.signal_quality;
    // MB2's monitoring nudge, sent BEFORE every per-arm hostname gate below:
    // the frames are id-only and `monitoring::sections` renders members from
    // whichever Hub stored them, so gating here would stop repainting a row
    // the page is showing. See `monitoring::names_for_device`.
    //
    // One device, never the fleet: this runs per open connection per
    // reading, and the batch arms need exactly the Hub the event names — a
    // `store.snapshot()` here cloned every device of every Hub on every
    // `sensors/aggregated` message times every open tab, to read one
    // device's key set (review round 2).
    let names = match &change {
        ChangeEvent::NodeBatch { hostname }
        | ChangeEvent::BeaconBatch { hostname }
        | ChangeEvent::DeviceStatus { hostname, .. } => {
            let device = store.snapshot_device(hostname);
            crate::monitoring::names_for_device(&change, device.as_ref())
        }
        // Every other arm derives its names from the event's own fields.
        other => crate::monitoring::names_for_device(other, None),
    };
    for name in names {
        if tx
            .send(
                Event::default()
                    .event(sse_event_name(name))
                    .data(NUDGE_PAYLOAD),
            )
            .await
            .is_err()
        {
            return false;
        }
    }

    // #87: from here on the event names the Hub the way the pages do. Before decision #21
    // one Hub arrived under two spellings — `fiber_app` uppercased `/etc/hostname`,
    // `control-radio` did not; an older agent still does — and every row, card and `tbody` is rendered with the store's
    // `DeviceState::hostname`. Left as the wire spelling, a `control-radio` reading named
    // `node-fiber-…` while the row listened for `node-FIBER-…`, so no live update ever
    // landed. After the nudge above on purpose: monitoring names follow
    // `sensor_group_members`, which keeps the wire spelling (d1eaa0e).
    let mut change = change;
    if let Some(hostname) = change.hostname_mut() {
        if let Some(canonical) = store.display_hostname(hostname) {
            *hostname = canonical;
        }
    }

    match change {
        // Id-only since the Hub Sensors tab (devices.hub.md §3.2): the row
        // refetches `GET /hubs/{hub}/sensors/{line}/row` itself, the same
        // inversion the monitoring rows use, so no markup travels here.
        ChangeEvent::SensorLine { hostname, line } => tx
            .send(
                Event::default()
                    .event(sse_event_name(format!("sensor-{hostname}-{line}")))
                    .data(NUDGE_PAYLOAD),
            )
            .await
            .is_ok(),
        ChangeEvent::DeviceStatus { hostname, .. }
        | ChangeEvent::System { hostname }
        | ChangeEvent::Power { hostname }
        | ChangeEvent::Network { hostname }
        | ChangeEvent::Storage { hostname }
        | ChangeEvent::Standby { hostname }
        | ChangeEvent::Config { hostname } => {
            debounce
                .entry(hostname)
                .or_insert_with(|| Instant::now() + DEBOUNCE_WINDOW);
            true
        }
        ChangeEvent::Alarm { .. } => true,

        ChangeEvent::NodeBatch { hostname } => {
            let Some(device) = store.snapshot_device(&hostname) else {
                return true;
            };
            let rows = memoed_row_data(&mut memos.node, state, &hostname).await;
            for (node_key, node) in &device.nodes {
                let ctx = rows.card(node, &device);
                let ok = send_rendered(
                    templates,
                    tx,
                    "partials/node_card.html",
                    minijinja::context! { hostname => hostname, can_edit => can_edit, n => ctx },
                    format!("node-{hostname}-{node_key}"),
                    lang,
                )
                .await;
                if !ok {
                    return false;
                }
            }
            true
        }
        ChangeEvent::Node { hostname, node_key } => {
            let Some(device) = store.snapshot_device(&hostname) else {
                return true;
            };
            let Some(node) = device.nodes.get(&node_key) else {
                return true;
            };
            let rows = memoed_row_data(&mut memos.node, state, &hostname).await;
            let ctx = rows.card(node, &device);
            send_rendered(
                templates,
                tx,
                "partials/node_card.html",
                minijinja::context! { hostname => hostname, can_edit => can_edit, n => ctx },
                format!("node-{hostname}-{node_key}"),
                lang,
            )
            .await
        }
        // B4: a removal deletes its row instead of reloading every open
        // tab. htmx applies `hx-swap-oob="delete"` to the element with that
        // id wherever it sits, so the payload is a stub `<tr>` carrying
        // nothing but the id — the `#node-rows` tbody the frame is
        // addressed to never has to make room for it.
        ChangeEvent::NodeRemoved { hostname, node_key } => {
            // Escaped, unlike every other fragment on this connection:
            // this is the one piece of HTML in the portal that minijinja
            // never sees, and `node_key` is an MQTT topic segment that
            // `parse_node_topic` only checks for non-emptiness — a segment
            // may legally carry `"` or `<`. Without this, a tombstone
            // published on a crafted topic would swap attacker markup into
            // every open operator tab.
            let id = format!(
                "node-{}-{}",
                crate::notify::payload::escape(&hostname),
                crate::notify::payload::escape(&node_key)
            );
            tx.send(
                Event::default()
                    .event(sse_event_name(format!("node-removed-{hostname}")))
                    .data(format!(r#"<tr id="{id}" hx-swap-oob="delete"></tr>"#)),
            )
            .await
            .is_ok()
        }
        // ...and a registration prepends one. Only a TRUE insert reaches
        // here (see `ChangeEvent::NodeAdded`), so the `afterbegin` swap
        // cannot grow the table on telemetry. The row is built by the same
        // `node_row_data` builder the page and every other row swap use, so
        // the prepended row is identical to the one a reload would draw.
        ChangeEvent::NodeAdded { hostname, node_key } => {
            let Some(device) = store.snapshot_device(&hostname) else {
                return true;
            };
            let Some(node) = device.nodes.get(&node_key) else {
                return true;
            };
            let rows = memoed_row_data(&mut memos.node, state, &hostname).await;
            let ctx = rows.card(node, &device);
            send_rendered(
                templates,
                tx,
                "partials/node_card.html",
                minijinja::context! { hostname => hostname, can_edit => can_edit, n => ctx },
                format!("node-added-{hostname}"),
                lang,
            )
            .await
        }
        // B4 gave this arm the two swap targets it had been waiting for:
        // `partials/node_config.html` (extracted from the detail page so it
        // could have an id at all) and the §3.5 banner, which has to
        // repaint here as well — a config page arriving from the device is
        // what turns the banner's "on the device" column from a guess into
        // a fact, and it is the moment after a Send when an operator most
        // wants to see it.
        ChangeEvent::NodeConfig { hostname, node_key } => {
            let ctx =
                crate::routes::node_routes::pending_ctx_for(state, &hostname, &node_key, can_edit)
                    .await;
            if !send_rendered(
                templates,
                tx,
                "partials/pending_config_body.html",
                minijinja::context! { pending => ctx },
                format!("pending-config-{node_key}"),
                lang,
            )
            .await
            {
                return false;
            }

            // The Diagnostics Device card: a Node's Info lands under this event too, so "Read
            // from the device" repaints it in place (nodes_phase1.cs.md WP7, found in WP4).
            let device = store.snapshot_device(&hostname);
            if let Some((device, node)) = device
                .as_ref()
                .and_then(|d| d.nodes.get(&node_key).map(|n| (d, n)))
            {
                let n = crate::render::node_detail_ctx(
                    node,
                    device.node_infos.get(&node_key),
                    device.node_device_infos.get(&node_key),
                    device.node_configs.get(&node_key),
                    None,
                    // Live fields only: the stream has no storage handle, and the page it
                    // patches already had the stored ones on its first render.
                    &node.fields.keys().cloned().collect(),
                )
                .with_device_status(device.node_device_statuses.get(&node_key))
                .with_last_fault(
                    crate::routes::node_routes::last_fault_event(
                        state,
                        &device.hostname,
                        &node_key,
                    )
                    .await
                    .as_ref(),
                );
                if !send_rendered(
                    templates,
                    tx,
                    "partials/node_device_body.html",
                    minijinja::context! {
                        n => n,
                        can_edit => can_edit,
                        config => crate::render::node_config_ctx(device.node_configs.get(&node_key)),
                    },
                    format!("node-device-{node_key}"),
                    lang,
                )
                .await
                {
                    return false;
                }
            }

            if device
                .and_then(|d| d.node_configs.get(&node_key).cloned())
                .is_none()
            {
                // The info tombstone rides this event too, and it clears a
                // cache the config table does not read. Nothing to repaint.
                return true;
            }
            send_config_body(state, templates, tx, &hostname, &node_key, can_edit, lang).await
        }
        ChangeEvent::NodeCommandResult {
            node_key, payload, ..
        } => {
            // GitLab #121: the banner leads with the Node's name, not its key.
            let target = crate::routes::command::target_label(state, &node_key).await;
            handle_node_command_result(&target, &payload, templates, lang, tx).await
        }
        ChangeEvent::BeaconBatch { hostname } => {
            // Two bulk queries for the whole fleet, not one per Beacon —
            // this arm and the next were two of §16.3's four N+1 sites. One
            // snapshot for both the fleet and this Hub's entry, for the
            // reason the single-Beacon arm below spells out.
            let snapshot = store.snapshot();
            let Some(device) = snapshot.device(&hostname) else {
                return true;
            };
            // Decision #42: in a cluster every registered Beacon's row lives under the Server,
            // whichever Hub's snapshot this is, and reads the cluster's view of it.
            let cluster_row_host = cluster_beacon_row_host(&snapshot);
            let row_host = cluster_row_host.clone().unwrap_or_else(|| hostname.clone());
            let rows = memoed_beacon_row_data(&mut memos.beacon, state, &row_host).await;
            for (mac, beacon) in &device.ble_sensors {
                let beacon = match (&snapshot.beacon_cluster, &cluster_row_host) {
                    (Some(cluster), Some(_)) if cluster.is_registered(mac) => {
                        match snapshot.hub_of_beacon(mac) {
                            Some((_, view)) => view,
                            None => continue,
                        }
                    }
                    (Some(_), _) => continue,
                    // Discovered-only candidates have no swap target — see
                    // module doc.
                    (None, _) if beacon.discovered == Some(true) => continue,
                    (None, _) => beacon,
                };
                let ctx =
                    render::beacon_ctx(rows.inputs(beacon, render::beacon_hubs(&snapshot, mac)));
                let mac_id = mac.replace(':', "-");
                let ok = send_rendered(
                    templates,
                    tx,
                    "partials/beacon_card.html",
                    minijinja::context! { hostname => row_host, can_edit => can_edit, b => ctx },
                    format!("beacon-{row_host}-{mac_id}"),
                    lang,
                )
                .await;
                if !ok {
                    return false;
                }
            }
            if snapshot.beacon_cluster.is_some() {
                // One merged scan panel for the whole cluster, strangers one row per MAC.
                let panel = render::cluster_discovered_panel(
                    &snapshot,
                    &signal_quality,
                    render::now_unix(),
                );
                return send_rendered(
                    templates,
                    tx,
                    "partials/discovered_table.html",
                    minijinja::context! { hostname => "cluster", can_edit => can_edit, d => panel },
                    CLUSTER_SCAN_EVENT.to_string(),
                    lang,
                )
                .await;
            }
            // B6/WP6.1: the scan panel, once per snapshot. It is a shell on
            // `/beacons/add` and nowhere else, so a connection with that page
            // closed simply has nothing to swap into — the same cheap-frame
            // stance every other named event in this module takes. One
            // render for the whole panel, not one per stranger: a candidate
            // appearing or vanishing changes the table's membership, which
            // is a re-render, not a row swap (which is why the module doc's
            // "no swap target for candidates" note no longer applies).
            let panel = render::discovered_panel(device, &signal_quality, render::now_unix());
            send_rendered(
                templates,
                tx,
                "partials/discovered_table.html",
                minijinja::context! { hostname => hostname, can_edit => can_edit, d => panel },
                format!("beacons-discovered-{hostname}"),
                lang,
            )
            .await
        }
        ChangeEvent::Beacon { hostname, mac } => {
            // One snapshot, read twice: `beacon_hubs` needs the whole state
            // (a Beacon is heard by every Hub in range) and the card needs
            // this Hub's own entry. Taking `snapshot_device` *and* a full
            // `snapshot` clones the store twice per advertisement and can
            // straddle two versions of it.
            let snapshot = store.snapshot();
            let Some(device) = snapshot.device(&hostname) else {
                return true;
            };
            let Some(beacon) = device.ble_sensors.get(&mac) else {
                return true;
            };
            // Decision #42: a cluster's registered Beacon renders once, under the Server, from
            // the cluster's view of it; a stranger has no row.
            let (hostname, beacon) =
                match (&snapshot.beacon_cluster, cluster_beacon_row_host(&snapshot)) {
                    (Some(cluster), Some(server)) if cluster.is_registered(&mac) => {
                        match snapshot.hub_of_beacon(&mac) {
                            Some((_, view)) => (server, view),
                            None => return true,
                        }
                    }
                    (Some(_), _) => return true,
                    (None, _) if beacon.discovered == Some(true) => return true,
                    (None, _) => (hostname, beacon),
                };
            let rows = memoed_beacon_row_data(&mut memos.beacon, state, &hostname).await;
            let ctx = render::beacon_ctx(rows.inputs(beacon, render::beacon_hubs(&snapshot, &mac)));
            let mac_id = mac.replace(':', "-");
            send_rendered(
                templates,
                tx,
                "partials/beacon_card.html",
                minijinja::context! { hostname => hostname, can_edit => can_edit, b => ctx },
                format!("beacon-{hostname}-{mac_id}"),
                lang,
            )
            .await
        }
        // A newly enrolled Beacon has no `beacon-{hostname}-{mac}` element on
        // any page yet, so its `Beacon` frame lands nowhere and `/beacons`
        // stayed without it until a manual reload. The mirror of the
        // deregistration arm below.
        ChangeEvent::BeaconAdded { .. } => tx
            .send(Event::default().event("refresh").data(NUDGE_PAYLOAD))
            .await
            .is_ok(),
        ChangeEvent::BeaconRemoved { discovered, .. } => {
            // A pruned **candidate** sends nothing: it exists on exactly one
            // page, the scan panel on `/beacons/add`, and the `BeaconBatch`
            // that accompanies every reconcile pass repaints that panel
            // whole. Reloading instead would wipe the half-typed registration
            // form above it every time a stranger walked out of range — which
            // reconcile-by-absence makes routine, not rare (B6 review, item
            // 2).
            if discovered {
                return true;
            }
            // An **enrolled** Beacon is a different matter: its card is a row
            // on `/beacons`, on the group pages and in the alarm lists, none
            // of which has a fragment for a card that should stop existing.
            tx.send(Event::default().event("refresh").data(NUDGE_PAYLOAD))
                .await
                .is_ok()
        }
        ChangeEvent::NodeHistoryProgress { .. } => true,
        // For the pending walk (P-7); a page has nothing to repaint for a join.
        ChangeEvent::NodeJoined { .. } => true,
        // devices.hub.md §6: the Hub page's "Radio & gateways" card. Id-only nudges, like
        // MB2's monitoring frames — the card's fragment and its route land with the step that
        // renders it, and the names are already what that step will listen on. Ungated on the
        // displayed hostname for the same reason the monitoring nudges are: the frame carries
        // no markup, and `radio-{hostname}` is the whole message.
        ChangeEvent::Radio { hostname } => tx
            .send(
                Event::default()
                    .event(sse_event_name(format!("radio-{hostname}")))
                    .data(NUDGE_PAYLOAD),
            )
            .await
            .is_ok(),
        // No hostname: one cluster-wide report, so one name every Hub page listens on.
        ChangeEvent::RadioGateways => tx
            .send(Event::default().event("radio-gateways").data(NUDGE_PAYLOAD))
            .await
            .is_ok(),
        // MB2: the group arrangement changed under every open tab. No
        // fragment covers it — a create or a delete adds or removes a whole
        // `<section>` — so this is the same page-level reload hint the
        // removal events use.
        ChangeEvent::Groups => tx
            .send(Event::default().event("refresh").data(NUDGE_PAYLOAD))
            .await
            .is_ok(),
        ChangeEvent::MqttConnection { .. } => {
            let mqtt = store.snapshot_mqtt();
            let ctx = render::mqtt_ctx(&mqtt);
            send_rendered(
                templates,
                tx,
                "partials/mqtt_banner.html",
                minijinja::context! { mqtt => ctx },
                "mqtt-status".to_string(),
                lang,
            )
            .await
        }
    }
}

/// `templates/partials/command_status.html`'s `cmd` context — field names
/// and shapes are that template's binding contract (see its own doc
/// comment).
#[derive(Debug, Clone, Serialize)]
struct CommandStatusCtx {
    request_id: String,
    command_type: String,
    /// The name of the device the command is about, the banner's first line
    /// (GitLab #121); `None` only for a request the relay did not publish.
    target: Option<String>,
    status: &'static str,
    message: Option<String>,
    error_code: Option<&'static str>,
    ts: String,
}

/// Renders one [`CommandStatus`] transition as `partials/command_status.html`,
/// SSE event `command-status`. Returns `false` once the client has
/// disconnected.
async fn handle_command_status(
    status: CommandStatus,
    target: Option<String>,
    templates: &Environment<'static>,
    lang: &'static str,
    tx: &mpsc::Sender<Event>,
) -> bool {
    // Decision #41: a limit is saved in the Portal before it is relayed. Its in-flight steps
    // would draw "Sending…" over the "Saved" the POST already answered, and a timeout or
    // refusal is only the device's copy missing, not a failed save.
    let mut status = status;
    if crate::routes::command::stored_first(&status.command_type) {
        match status.status {
            "success" | "delivered" | "stored" => {}
            "error" | "timeout" => {
                status.status = "stored";
                status.error_code = Some("device_copy_not_delivered");
            }
            _ => return true,
        }
    }
    let ctx = CommandStatusCtx {
        request_id: status.request_id,
        command_type: status.command_type,
        target,
        status: status.status,
        message: status.message,
        error_code: status.error_code,
        ts: render::format_time(status.ts),
    };
    send_rendered(
        templates,
        tx,
        "partials/command_status.html",
        minijinja::context! { cmd => ctx },
        "command-status".to_string(),
        lang,
    )
    .await
}

/// Renders one raw `…/{dev_eui}/command` result (`ChangeEvent::NodeCommandResult`)
/// as `partials/command_status.html`, SSE event `command-status` — see the
/// module doc for why this doesn't double-report on top of relay's
/// `CommandStatus` stream. `request_id` is always empty (the raw payload
/// carries none); `command_type` folds in `node_key` so the shared
/// `#command-status` singleton stays legible with more than one Node.
async fn handle_node_command_result(
    target: &str,
    payload: &serde_json::Value,
    templates: &Environment<'static>,
    lang: &'static str,
    tx: &mpsc::Sender<Event>,
) -> bool {
    let command = payload
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("sticker_command");
    let result = payload.get("result").and_then(serde_json::Value::as_str);
    let status: &'static str = match result {
        Some(r) if NODE_COMMAND_OK_RESULTS.contains(&r) => "success",
        Some(_) => "error",
        // Unrecognized/missing shape — no evidence of failure, don't guess one.
        None => "success",
    };
    let message = payload
        .get("detail")
        .and_then(serde_json::Value::as_str)
        .or_else(|| payload.get("message").and_then(serde_json::Value::as_str))
        .or_else(|| payload.get("error").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .or_else(|| result.map(str::to_string));

    let ctx = CommandStatusCtx {
        request_id: String::new(),
        command_type: command.to_string(),
        target: Some(target.to_string()),
        status,
        message,
        // A result outside the OK list is the Node's own refusal, unless it
        // is one control-core publishes without the Node saying anything.
        error_code: (status == "error").then(|| {
            use crate::relay::error_code::*;
            match result {
                Some(TRANSPORT_ERROR) => TRANSPORT_ERROR,
                // The Hub gave the downlink up (§10.5, amended 2026-10-07): not a refusal.
                // Discarded before it was sent (#129): the reason, not the give-up sentence.
                Some(UNDELIVERED) if payload.get("reason").is_some() => NOT_SENT,
                Some(UNDELIVERED) => UNDELIVERED,
                Some(RATE_LIMITED) => RATE_LIMITED,
                Some(DEVICE_BUSY) => DEVICE_BUSY,
                _ => DEVICE_REJECTED,
            }
        }),
        ts: render::format_time(now_unix()),
    };
    send_rendered(
        templates,
        tx,
        "partials/command_status.html",
        minijinja::context! { cmd => ctx },
        "command-status".to_string(),
        lang,
    )
    .await
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Renders and sends every hostname whose debounce window has expired.
/// Returns `false` once the client has disconnected.
async fn flush_due(
    store: &StateStore,
    templates: &Environment<'static>,
    can_edit: bool,
    lang: &'static str,
    tx: &mpsc::Sender<Event>,
    pending: &mut HashMap<String, Instant>,
) -> bool {
    let now = Instant::now();
    let due: Vec<String> = pending
        .iter()
        .filter(|(_, deadline)| **deadline <= now)
        .map(|(hostname, _)| hostname.clone())
        .collect();

    for hostname in due {
        pending.remove(&hostname);
        // The device may have left the store since the event was queued.
        let Some(device) = store.snapshot_device(&hostname) else {
            continue;
        };
        let ctx = render::device_status_ctx(&device, can_edit);
        let ok = send_rendered(
            templates,
            tx,
            "partials/device_status.html",
            minijinja::context! { device => ctx },
            format!("device-{hostname}-status"),
            lang,
        )
        .await;
        if !ok {
            return false;
        }
    }
    true
}

/// Renders `template_name` with `ctx` and sends it as a named SSE event.
/// A render failure is logged and the frame is dropped rather than sent
/// broken — the next change event (or a `refresh` on lag) will retry the
/// fragment. Returns `false` only when the client has disconnected.
///
/// M9: this is the SSE fragment path's single merge point for `lang` — see
/// `i18n.rs`'s module doc for why this path (unlike `routes::render()`)
/// takes it as a plain parameter instead of reading the per-request
/// task-local.
async fn send_rendered(
    templates: &Environment<'static>,
    tx: &mpsc::Sender<Event>,
    template_name: &str,
    ctx: Value,
    event_name: String,
    lang: &'static str,
) -> bool {
    let ctx = minijinja::context! { lang => lang, ..ctx };
    let rendered = templates
        .get_template(template_name)
        .and_then(|tmpl| tmpl.render(ctx));

    let html = match rendered {
        Ok(html) => html,
        Err(err) => {
            tracing::warn!(%err, template_name, "failed to render SSE fragment; dropping");
            return true;
        }
    };

    tx.send(
        Event::default()
            .event(sse_event_name(event_name))
            .data(html),
    )
    .await
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Node command result names why it failed: the three refusals
    /// control-core publishes itself (monitor.rs `transport_error`,
    /// `rate_limited`, `device_busy`) get their own sentence, and only a
    /// result from the device reads "The device rejected the command".
    #[tokio::test]
    async fn a_node_command_result_says_which_failure_it_was() {
        let templates = crate::render::Templates::new("templates", "static");
        let env = templates.env();
        let frame = |result: &'static str| {
            let env = env.clone();
            async move {
                let (tx, mut rx) = mpsc::channel(4);
                let payload = serde_json::json!({ "command": "force_send", "result": result });
                assert!(handle_node_command_result("aa11", &payload, &env, "en", &tx).await);
                format!("{:?}", rx.recv().await.expect("a frame"))
            }
        };
        for (result, sentence) in [
            ("transport_error", "The Hub got no answer from the Node"),
            ("rate_limited", "was sent to this Node moments ago"),
            ("device_busy", "still handling an earlier command"),
            ("invalid_param", "The device rejected the command."),
        ] {
            let html = frame(result).await;
            assert!(html.contains(sentence), "{result}: {html}");
            if result != "invalid_param" {
                assert!(!html.contains("rejected"), "{result}: {html}");
            }
        }
    }

    /// #129: an `undelivered` result says which kind it was. A P2P give-up keeps its
    /// attempts sentence; a frame the network server discarded says it could not be sent and
    /// shows why, without the "0 attempts" detail.
    #[tokio::test]
    async fn an_undelivered_result_with_a_reason_says_why_it_was_not_sent() {
        let templates = crate::render::Templates::new("templates", "static");
        let env = templates.env();
        let render = |payload: serde_json::Value, lang: &'static str| {
            let env = env.clone();
            async move {
                let (tx, mut rx) = mpsc::channel(4);
                assert!(handle_node_command_result("aa11", &payload, &env, lang, &tx).await);
                format!("{:?}", rx.recv().await.expect("a frame"))
            }
        };
        let reason = "discarded by the network server: 88 B exceeds 51 B at the current data rate";
        let discarded = serde_json::json!({
            "command": "undelivered", "result": "undelivered", "seq": 13,
            "detail": reason, "reason": reason,
        });
        let html = render(discarded.clone(), "en").await;
        assert!(html.contains("could not be sent"), "{html}");
        assert!(html.contains(reason), "{html}");
        assert!(!html.contains("attempts"), "{html}");
        let html = render(discarded, "cs").await;
        // The frame is `{:?}`-printed, so only the ASCII part of "Příkaz nelze odeslat" matches.
        assert!(html.contains("kaz nelze odeslat:"), "{html}");

        let gave_up = serde_json::json!({
            "command": "undelivered", "result": "undelivered", "seq": 31,
            "detail": "5 attempts, last NotDelivered",
        });
        let html = render(gave_up, "en").await;
        assert!(html.contains("in 5 attempts and gave it up"), "{html}");
        assert!(html.contains("5 attempts, last NotDelivered"), "{html}");
        assert!(!html.contains("could not be sent"), "{html}");
    }

    /// WP4: a Node's Info repaints the Diagnostics Device card in place, so "Read from the
    /// device" shows its answer without a reload.
    #[tokio::test]
    async fn a_device_info_repaints_the_device_card() {
        use crate::state::events::{IngestEvent, NodeReadingMsg};
        let (state, _dir) = crate::routes::test_support::test_state().await;
        state.store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Box::new(NodeReadingMsg {
                message_id: "m-aa11".into(),
                dev_eui: "aa11".into(),
                serial: None,
                fields: [("temperature".to_string(), 21.5)].into_iter().collect(),
                counters: Default::default(),
                events: Vec::new(),
                gateways: Vec::new(),
                rssi: None,
                snr: None,
                fport: Some(2),
                page_index: None,
                page_count: None,
                dr: None,
                received_at: "2026-09-24T12:58:36Z".into(),
            }),
        });

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (tx, mut rx) = mpsc::channel(64);
        let store = state.store.clone();
        let task = tokio::spawn(run(
            AppState {
                shutdown: shutdown_rx,
                ..state
            },
            true,
            "en",
            tx,
        ));

        let info: crate::state::events::NodeDeviceInfoMsg =
            serde_json::from_value(serde_json::json!({
                "dev_eui": "aa11",
                "received_at": "2026-09-24T12:58:38Z",
                "device_info": {"fw_major": 1, "fw_minor": 5, "fw_patch": 0, "uptime_s": 6645,
                                "serial_number": 2162190413_u64, "active_alarms": []},
            }))
            .expect("device info document");
        let mut frames = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !frames
            .iter()
            .any(|f: &String| f.contains("node-device-aa11"))
        {
            // `run` subscribes once it starts; re-apply until a frame proves it is live.
            store.apply(IngestEvent::NodeDeviceInfo {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Some(Box::new(info.clone())),
            });
            if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await
            {
                frames.push(format!("{ev:?}"));
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no Device card frame: {frames:?}"
            );
        }
        let frame = frames
            .iter()
            .find(|f| f.contains("node-device-aa11"))
            .expect("frame");
        assert!(frame.contains("1 h 50 min"), "the new uptime: {frame}");
        assert!(frame.contains("2162190413"), "the serial number");
        // The Refresh button is in the card header, outside the swap target.
        assert!(frame.contains("ConfigDump"), "the boot settings group");
        task.abort();
    }

    /// `ChangeEvent::Beacon` fires per advertisement, and its arm used to
    /// run two fleet-wide reads every time, per open connection. The Node
    /// side has been memoed since §21 review item 7; this proves the Beacon
    /// side now is, and that the memo is still keyed by hostname rather
    /// than being a cache that would serve one Hub's rows for another's.
    #[tokio::test]
    async fn the_beacon_memo_collapses_a_burst_but_not_two_hubs() {
        let (state, _dir) = crate::routes::test_support::test_state().await;
        let mut memo: BeaconRowDataMemo = None;

        memoed_beacon_row_data(&mut memo, &state, "h1").await;
        let first = memo.as_ref().expect("filled").loaded_at;

        // A second advertisement inside the TTL reuses the same load.
        memoed_beacon_row_data(&mut memo, &state, "h1").await;
        assert_eq!(
            memo.as_ref().expect("still filled").loaded_at,
            first,
            "a burst for one Hub is one bulk load, not one per advertisement"
        );

        // A different Hub is a different answer and must refill.
        memoed_beacon_row_data(&mut memo, &state, "h2").await;
        let entry = memo.as_ref().expect("refilled");
        assert_eq!(entry.hostname, "h2");
        assert_ne!(
            entry.loaded_at, first,
            "the memo is keyed by hostname, not shared across Hubs"
        );
    }

    /// An `AlarmNudge` carrying `activated_ids` must yield the re-rendered
    /// alarm table AND one `alarm-toast` fragment per id. The toast rides a
    /// template no page ever renders (`partials/alarm_toast.html` exists
    /// only as an SSE fragment), so a template error there is invisible to
    /// every handler test — `send_rendered` would just log-and-drop it.
    /// This is the test that turns that silent drop into a failure.
    #[tokio::test]
    async fn activated_nudge_sends_a_toast_fragment() {
        let (state, dir) = crate::routes::test_support::test_state().await;

        let storage = portal_storage::Storage::open(&dir.path().join("portal.db"), None)
            .expect("open storage");
        let id = storage
            .insert_alarm_event(&portal_storage::NewAlarmEvent {
                hostname: "h1".into(),
                sensor_id: "line-0".into(),
                sensor_name: Some("Fridge".into()),
                sensor_location: Some("kitchen".into()),
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

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (tx, mut rx) = mpsc::channel(16);
        let nudge_tx = state.alarms.nudge_sender();
        let task = tokio::spawn(run(
            AppState {
                shutdown: shutdown_rx,
                ..state
            },
            false,
            "en",
            tx,
        ));

        // `run` subscribes to the nudge broadcast only once it starts, and a
        // broadcast send before that is simply lost — so re-send until the
        // first frame proves the subscription is live.
        let mut frames = Vec::new();
        for _ in 0..50 {
            let _ = nudge_tx.send(AlarmNudge {
                hostname: "h1".to_string(),
                activated_ids: vec![id.clone()],
                escalated_ids: Vec::new(),
            });
            if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await
            {
                frames.push(format!("{ev:?}"));
                break;
            }
        }
        assert!(!frames.is_empty(), "no SSE frame arrived for the nudge");

        // One nudge produces alarm-table then alarm-toast; collect until the
        // toast shows up (earlier re-sent nudges may add extra table frames).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !frames.iter().any(|f| f.contains("alarm-toast")) {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .expect("alarm-toast frame never arrived");
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(ev)) => frames.push(format!("{ev:?}")),
                _ => panic!("stream ended before an alarm-toast frame arrived"),
            }
        }

        let toast = frames
            .iter()
            .find(|f| f.contains("alarm-toast"))
            .expect("checked in the loop above");
        // The rendered fragment carries the cause, sensor, value+threshold
        // and severity color the toast contract promises.
        for needle in [
            // The type's label, uppercased by CSS (HS-13 made the English one readable).
            "High value",
            "Fridge",
            "kitchen",
            "15.7",
            "12",
            "alert-error",
            &id,
        ] {
            assert!(
                toast.contains(needle),
                "toast fragment missing {needle:?}: {toast}"
            );
        }
        assert!(
            !toast.contains("escalated"),
            "an activation toast must not carry the escalation label: {toast}"
        );

        // An escalation nudge renders the same fragment with the escalation
        // label (subscription already proven live above, one send suffices).
        let _ = nudge_tx.send(AlarmNudge {
            hostname: "h1".to_string(),
            activated_ids: Vec::new(),
            escalated_ids: vec![id.clone()],
        });
        let escalation_toast = loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("no escalation frame within 5s")
                .expect("stream ended before the escalation toast");
            let frame = format!("{ev:?}");
            if frame.contains("alarm-toast") {
                break frame;
            }
        };
        assert!(
            escalation_toast.contains("escalated"),
            "escalation toast missing its label: {escalation_toast}"
        );

        drop(_shutdown_tx);
        task.abort();
    }

    // ---- MB2: the monitoring nudge -----------------------------------

    /// Drives `run` against a live store and returns every frame that
    /// arrives within `window`, formatted the way the harness above reads
    /// them (`Debug` carries both the event name and the data).
    ///
    /// `run` subscribes only once it starts, and a broadcast send before
    /// that is simply lost — so `seed` is re-run until the first frame
    /// proves the subscription is live, the same trick the alarm test uses.
    async fn frames_for(
        state: crate::http::AppState,
        seed: impl Fn(),
        window: Duration,
    ) -> Vec<String> {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (tx, mut rx) = mpsc::channel(64);
        let task = tokio::spawn(run(
            AppState {
                shutdown: shutdown_rx,
                ..state
            },
            false,
            "en",
            tx,
        ));

        let mut frames = Vec::new();
        for _ in 0..50 {
            seed();
            if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await
            {
                frames.push(format!("{ev:?}"));
                break;
            }
        }
        // Drain whatever else the same seed produced.
        while let Ok(Some(ev)) = tokio::time::timeout(window, rx.recv()).await {
            frames.push(format!("{ev:?}"));
        }

        drop(_shutdown_tx);
        task.abort();
        frames
    }

    /// Frames carrying exactly this event name. `Event`'s `Debug` prints the
    /// wire buffer, so the trailing newline is what keeps `mon-node-h1-n1`
    /// from also matching `mon-node-h1-n11`.
    fn named(frames: &[String], event: &str) -> Vec<String> {
        let needle = format!("event: {event}\\n");
        frames
            .iter()
            .filter(|f| f.contains(&needle))
            .cloned()
            .collect()
    }

    /// The nudge contract: a frame that names an entity and carries no
    /// markup — [`NUDGE_PAYLOAD`] and nothing else. It is one byte rather
    /// than zero because a browser drops an empty-data frame outright; see
    /// that constant.
    ///
    /// The needle is spelled out (`data: 1\n`) rather than interpolated from
    /// the constant: a test that formats the constant into its own
    /// expectation passes for *any* value of it, including the empty string
    /// this exists to rule out (monitoring.backend.md §15.2).
    fn carries_no_markup(frame: &str) -> bool {
        frame.contains("data: 1\\n") && !frame.contains('<')
    }

    /// #87 under decision #21: the store holds one lowercase spelling, the one the page draws
    /// its rows and `tbody` with. An older agent still names the Hub uppercase; both the live
    /// row update and the registration insert have to come out under the page's spelling,
    /// or no element ever takes them.
    #[tokio::test]
    async fn node_events_are_named_after_the_hub_the_page_drew() {
        let (state, _dir) = crate::routes::test_support::test_state().await;
        state
            .store
            .apply(crate::state::events::IngestEvent::Status {
                hostname: "FIBER-4E970F4A".into(),
                status: fiber_protocol::payloads::status::DeviceStatus::Online,
            });
        state
            .store
            .apply(crate::state::events::IngestEvent::NodeReading {
                hostname: "fiber-4e970f4a".into(),
                node_key: "n1".into(),
                payload: Box::new(crate::state::events::NodeReadingMsg {
                    message_id: "m-n1".into(),
                    dev_eui: "n1".into(),
                    serial: None,
                    fields: [("temperature".to_string(), 4.0)].into_iter().collect(),
                    counters: Default::default(),
                    events: Vec::new(),
                    gateways: Vec::new(),
                    rssi: Some(-70),
                    snr: Some(8.0),
                    fport: None,
                    page_index: None,
                    page_count: None,
                    dr: None,
                    received_at: "2026-09-22T21:36:47Z".to_string(),
                }),
            });
        assert_eq!(
            state.store.display_hostname("FIBER-4E970F4A").as_deref(),
            Some("fiber-4e970f4a")
        );

        let store = state.store.clone();
        let frames = frames_for(
            state,
            move || {
                store.emit(ChangeEvent::Node {
                    hostname: "FIBER-4E970F4A".into(),
                    node_key: "n1".into(),
                });
                store.emit(ChangeEvent::NodeAdded {
                    hostname: "FIBER-4E970F4A".into(),
                    node_key: "n1".into(),
                });
            },
            Duration::from_millis(300),
        )
        .await;

        assert!(
            !named(&frames, "node-fiber-4e970f4a-n1").is_empty(),
            "the row update must use the page's spelling: {frames:?}"
        );
        let added = named(&frames, "node-added-fiber-4e970f4a");
        assert!(
            !added.is_empty(),
            "the insert must use the page's spelling: {frames:?}"
        );
        // The inserted row carries the same id a reload draws, so the next update finds it.
        assert!(
            added.iter().all(|f| f.contains("node-fiber-4e970f4a-n1")),
            "{added:?}"
        );
        assert!(
            named(&frames, "node-FIBER-4E970F4A-n1").is_empty(),
            "{frames:?}"
        );
        assert!(
            named(&frames, "node-added-FIBER-4E970F4A").is_empty(),
            "{frames:?}"
        );
    }

    /// A store holding one Hub with two Nodes, so a batch has something to
    /// fan out over.
    async fn state_with_two_nodes() -> (crate::http::AppState, tempfile::TempDir) {
        let (state, dir) = crate::routes::test_support::test_state().await;
        for key in ["n1", "n2"] {
            state
                .store
                .apply(crate::state::events::IngestEvent::NodeReading {
                    hostname: "h1".into(),
                    node_key: key.into(),
                    payload: Box::new(crate::state::events::NodeReadingMsg {
                        message_id: format!("m-{key}"),
                        dev_eui: key.into(),
                        serial: None,
                        fields: [("temperature".to_string(), 4.0)].into_iter().collect(),
                        counters: Default::default(),
                        events: Vec::new(),
                        gateways: Vec::new(),
                        rssi: Some(-70),
                        snr: Some(8.0),
                        fport: None,
                        page_index: None,
                        page_count: None,
                        dr: None,
                        received_at: "2026-09-07T12:00:00Z".to_string(),
                    }),
                });
        }
        (state, dir)
    }

    /// MB2's contract in one assertion: the frame names the entity and
    /// carries **no markup**. A payload here would be one rendering for
    /// every viewer, and `/monitoring` renders in one of four layouts chosen
    /// per operator — the emptiness is what lets each browser refetch its
    /// own row in its own layout.
    #[tokio::test]
    async fn a_node_change_nudges_the_monitoring_row_with_an_empty_frame() {
        let (state, _dir) = state_with_two_nodes().await;
        let store = state.store.clone();
        let frames = frames_for(
            state,
            move || {
                store.emit(ChangeEvent::Node {
                    hostname: "h1".into(),
                    node_key: "n1".into(),
                })
            },
            Duration::from_millis(300),
        )
        .await;

        let nudges = named(&frames, "mon-node-h1-n1");
        assert!(!nudges.is_empty(), "no mon- frame arrived: {frames:?}");
        assert!(
            nudges.iter().all(|f| carries_no_markup(f)),
            "the nudge must carry no markup: {nudges:?}"
        );
        assert!(
            named(&frames, "refresh").is_empty(),
            "a plain reading must not reload every tab: {frames:?}"
        );
    }

    /// UX-020: a burst of readings becomes ONE `page-stats` nudge per
    /// [`STATS_WINDOW`], id-only like the row nudges — each tab refetches the
    /// metric row for the page it is on.
    #[tokio::test]
    async fn a_burst_of_changes_sends_one_debounced_stats_nudge() {
        let (state, _dir) = state_with_two_nodes().await;
        let store = state.store.clone();
        let frames = frames_for(
            state,
            move || {
                for key in ["n1", "n2", "n1", "n2"] {
                    store.emit(ChangeEvent::Node {
                        hostname: "h1".into(),
                        node_key: key.into(),
                    });
                }
            },
            STATS_WINDOW + Duration::from_millis(500),
        )
        .await;

        let nudges = named(&frames, "page-stats");
        assert_eq!(nudges.len(), 1, "{frames:?}");
        assert!(carries_no_markup(&nudges[0]), "{nudges:?}");
    }

    /// A batch fans out to one nudge per Node, the same way it already fans
    /// out to one card render each: rows are independently addressed, so
    /// there is no combined fragment to send instead.
    #[tokio::test]
    async fn a_node_batch_nudges_every_node_row() {
        let (state, _dir) = state_with_two_nodes().await;
        let store = state.store.clone();
        let frames = frames_for(
            state,
            move || {
                store.emit(ChangeEvent::NodeBatch {
                    hostname: "h1".into(),
                })
            },
            Duration::from_millis(300),
        )
        .await;

        for key in ["n1", "n2"] {
            assert!(
                !named(&frames, &format!("mon-node-h1-{key}")).is_empty(),
                "no nudge for {key}: {frames:?}"
            );
        }
    }

    /// The fault repaint has no inbound MQTT message behind it: the sweep
    /// notices a silence and `OfflineService::announce` emits the
    /// `ChangeEvent` that produces the nudge. This is the end-to-end guard
    /// for that path — MB2 deliberately added no second subscription to the
    /// transition broadcast, so if `announce` ever stopped emitting, this is
    /// what would fail rather than the fault triangle silently freezing.
    #[tokio::test]
    async fn a_liveness_transition_nudges_the_monitoring_row() {
        let (state, _dir) = crate::routes::test_support::test_state().await;
        // `store_with_a_node`'s reading, at the fixed timestamp the offline
        // tests use; no config for this Node, so the 15-minute fallback
        // applies — 4 x 900 s of budget.
        state
            .store
            .apply(crate::state::events::IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                payload: Box::new(crate::state::events::NodeReadingMsg {
                    message_id: "m1".into(),
                    dev_eui: "aa11".into(),
                    serial: None,
                    fields: [("temperature".to_string(), 4.0)].into_iter().collect(),
                    counters: Default::default(),
                    events: Vec::new(),
                    gateways: Vec::new(),
                    rssi: Some(-70),
                    snr: Some(8.0),
                    fport: None,
                    page_index: None,
                    page_count: None,
                    dr: None,
                    received_at: "2026-09-07T12:00:00Z".to_string(),
                }),
            });
        // `received_at` above, as unix seconds — the detector parses the
        // RFC3339 string the store holds, so the sweep's `now` has to be
        // expressed against the same instant.
        const SEEN_AT: i64 = 1_788_782_400; // 2026-09-07T12:00:00Z

        let offline = state.offline.clone();
        let store = state.store.clone();
        let frames = frames_for(
            state,
            move || {
                let offline = offline.clone();
                let store = store.clone();
                // The detector only transitions once; re-running the pass is
                // harmless because the second one produces nothing.
                tokio::spawn(async move { offline.pass(&store, SEEN_AT + 3_601).await });
            },
            Duration::from_millis(500),
        )
        .await;

        assert!(
            !named(&frames, "mon-node-h1-aa11").is_empty(),
            "a sweep that found a Node silent must repaint its group row: {frames:?}"
        );
    }

    /// Group CRUD has no fragment worth swapping — a create or a delete adds
    /// or removes a whole section — so the other open tabs reload.
    #[tokio::test]
    async fn a_group_change_refreshes_the_other_sessions() {
        let (state, _dir) = crate::routes::test_support::test_state().await;
        let store = state.store.clone();
        let frames = frames_for(
            state,
            move || store.emit(ChangeEvent::Groups),
            Duration::from_millis(200),
        )
        .await;

        let refreshes = named(&frames, "refresh");
        assert!(!refreshes.is_empty(), "no refresh frame: {frames:?}");
        assert!(refreshes.iter().all(|f| carries_no_markup(f)));
    }

    /// B6/WP6.1: every `eye/sensors` snapshot repaints the whole scan panel
    /// into `/beacons/add`'s shell.
    ///
    /// One frame for the table and not one per stranger — a candidate
    /// appearing or vanishing is a membership change, and there is no row
    /// to swap. The frame must carry markup (unlike the `mon-*` nudges),
    /// because the shell has nothing else to fetch it with.
    #[tokio::test]
    async fn a_snapshot_repaints_the_discovered_panel() {
        let (state, _dir) = crate::routes::test_support::test_state().await;
        let store = state.store.clone();
        let frames = frames_for(
            state,
            move || {
                store.apply(crate::state::events::IngestEvent::EyeSensors {
                    hostname: "h1".into(),
                    payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                        timestamp: None,
                        discovered_capped: false,
                        tags: vec![fiber_protocol::payloads::eye::EyeTag {
                            gateway: None,
                            mac: "AA:BB:CC:DD:EE:02".into(),
                            name: None,
                            temperature_c: Some(4.5),
                            humidity_pct: Some(51),
                            battery_mv: None,
                            low_battery: None,
                            magnet_present: None,
                            magnet_detected: None,
                            moving: None,
                            movement_count: None,
                            pitch_deg: None,
                            roll_deg: None,
                            rssi: Some(-55),
                            last_seen_ts: None,
                            stale: None,
                            provisioning: None,
                            discovered: Some(true),
                            field_alarm_states: std::collections::HashMap::new(),
                            alarm_state: None,
                            ble_config: None,
                        }],
                    },
                });
            },
            Duration::from_millis(500),
        )
        .await;

        let panel = named(&frames, "beacons-discovered-h1");
        assert!(!panel.is_empty(), "no scan-panel frame: {frames:?}");
        assert_eq!(panel.len(), 1, "one frame per snapshot, not one per row");
        assert!(
            panel[0].contains("AA:BB:CC:DD:EE:02"),
            "the panel must carry the stranger it repainted: {}",
            panel[0]
        );
        // A discovered-only candidate still gets no per-card frame: it is
        // not a row on /beacons.
        assert!(
            named(&frames, "beacon-h1-AA-BB-CC-DD-EE-02").is_empty(),
            "a candidate has no beacon card: {frames:?}"
        );
    }

    /// Decision #42: a Client's snapshot repaints the cluster's rows — under the Server, where
    /// the list keys them — and the one merged scan panel, never a per-Client row or panel.
    #[tokio::test]
    async fn a_client_snapshot_repaints_the_servers_row_and_the_cluster_scan() {
        let (state, _dir) = crate::routes::test_support::test_state().await;
        let store = state.store.clone();
        let tag = |mac: &str, discovered: Option<bool>| fiber_protocol::payloads::eye::EyeTag {
            discovered,
            rssi: Some(-60),
            ..crate::routes::beacon_routes::tests::test_eye_tag(mac)
        };
        store.apply(crate::state::events::IngestEvent::EyeSensors {
            hostname: "s1".into(),
            payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                timestamp: None,
                discovered_capped: false,
                tags: vec![tag("AA:BB:CC:DD:EE:01", None)],
            },
        });
        store.apply(crate::state::events::IngestEvent::EyeHomes {
            hostname: "s1".into(),
            payload: fiber_protocol::payloads::eye::EyeHomesMsg {
                v: 1,
                server: "s1".into(),
                registered: vec!["AA:BB:CC:DD:EE:01".into()],
                homes: [("AA:BB:CC:DD:EE:01".to_string(), "c1".to_string())].into(),
            },
        });
        let frames = frames_for(
            state,
            move || {
                store.apply(crate::state::events::IngestEvent::EyeSensors {
                    hostname: "c1".into(),
                    payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                        timestamp: None,
                        discovered_capped: false,
                        tags: vec![
                            tag("AA:BB:CC:DD:EE:01", None),
                            tag("AA:BB:CC:DD:EE:02", Some(true)),
                        ],
                    },
                });
            },
            Duration::from_millis(500),
        )
        .await;

        assert!(
            !named(&frames, "beacon-s1-AA-BB-CC-DD-EE-01").is_empty(),
            "the Server's row: {frames:?}"
        );
        assert!(named(&frames, "beacon-c1-AA-BB-CC-DD-EE-01").is_empty());
        let panel = named(&frames, CLUSTER_SCAN_EVENT);
        assert!(!panel.is_empty(), "the cluster scan: {frames:?}");
        assert!(panel[0].contains("AA:BB:CC:DD:EE:02"));
        assert!(named(&frames, "beacons-discovered-c1").is_empty());
    }

    /// Puts device `h1` in the store without emitting anything a subscriber
    /// would see, so the beacon arms have a device to snapshot.
    fn seed_h1(store: &crate::state::StateStore) {
        store.apply(crate::state::events::IngestEvent::EyeSensors {
            hostname: "h1".into(),
            payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                timestamp: None,
                discovered_capped: false,
                tags: vec![],
            },
        });
    }

    /// B6 review, item 2. `/beacons/add` carries a registration form the
    /// operator is typing into, directly above a panel that repaints itself.
    /// A stranger walking out of range must therefore not reload the page —
    /// while an operator deregistering a Beacon still must, because that
    /// card is a row on pages with no fragment for it.
    #[tokio::test]
    async fn a_pruned_candidate_sends_no_reload_but_a_deregistered_beacon_does() {
        let (state, _dir) = crate::routes::test_support::test_state().await;
        seed_h1(&state.store);
        let store = state.store.clone();
        // The pair the store really emits for a prune: the removal and the
        // `BeaconBatch` from the same snapshot. The panel repaint is what
        // makes the reload unnecessary, so the two are asserted together.
        let frames = frames_for(
            state,
            move || {
                store.emit(ChangeEvent::BeaconRemoved {
                    hostname: "h1".into(),
                    mac: "AA:BB:CC:DD:EE:02".into(),
                    discovered: true,
                });
                store.emit(ChangeEvent::BeaconBatch {
                    hostname: "h1".into(),
                });
            },
            Duration::from_millis(200),
        )
        .await;
        assert!(
            !named(&frames, "beacons-discovered-h1").is_empty(),
            "the scan panel did not repaint, so the reload would be load-bearing: {frames:?}"
        );
        assert!(
            named(&frames, "refresh").is_empty(),
            "a pruned candidate reloaded the scan page: {frames:?}"
        );

        let (state, _dir) = crate::routes::test_support::test_state().await;
        seed_h1(&state.store);
        let store = state.store.clone();
        let frames = frames_for(
            state,
            move || {
                store.emit(ChangeEvent::BeaconRemoved {
                    hostname: "h1".into(),
                    mac: "AA:BB:CC:DD:EE:01".into(),
                    discovered: false,
                });
            },
            Duration::from_millis(200),
        )
        .await;
        assert!(
            !named(&frames, "refresh").is_empty(),
            "a deregistered Beacon left a stale card behind: {frames:?}"
        );
    }

    /// A Beacon registered from `/beacons/add` (or the Hub) has no card on
    /// an open `/beacons` page yet, so only a reload shows it.
    #[tokio::test]
    async fn a_newly_enrolled_beacon_reloads_the_list() {
        let (state, _dir) = crate::routes::test_support::test_state().await;
        seed_h1(&state.store);
        let store = state.store.clone();
        let frames = frames_for(
            state,
            move || {
                store.emit(ChangeEvent::BeaconAdded {
                    hostname: "h1".into(),
                    mac: "AA:BB:CC:DD:EE:03".into(),
                });
            },
            Duration::from_millis(200),
        )
        .await;
        assert!(
            !named(&frames, "refresh").is_empty(),
            "a newly enrolled Beacon did not reload the list: {frames:?}"
        );
    }

    /// `/limits` has no fragment for a Node limit's sync state, so a moved draft has to
    /// nudge it to reload.
    #[tokio::test]
    async fn a_moved_draft_nudges_the_limits_page() {
        let (state, _dir) = crate::routes::test_support::test_state().await;
        seed_h1(&state.store);
        let pending = state.pending.clone();
        let frames = frames_for(
            state,
            move || pending.nudge("h1", "aa11"),
            Duration::from_millis(200),
        )
        .await;
        assert!(
            !named(&frames, "limits-pending").is_empty(),
            "no limits-pending nudge: {frames:?}"
        );
    }

    /// A shutdown signal must end the stream. This is not cosmetic: axum's
    /// graceful shutdown waits for in-flight responses, and an SSE response
    /// never finishes on its own, so without this the process hangs until
    /// systemd SIGKILLs it — with one browser tab enough to cause it. The
    /// closed channel is what finishes the response body.
    #[tokio::test]
    async fn shutdown_ends_the_stream() {
        let (state, _dir) = crate::routes::test_support::test_state().await;
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (tx, mut rx) = mpsc::channel(4);

        let task = tokio::spawn(run(
            AppState {
                shutdown: shutdown_rx,
                ..state
            },
            false,
            "en",
            tx,
        ));

        shutdown_tx.send(true).expect("signal shutdown");

        // `recv() == None` means the sender was dropped, i.e. `run` returned.
        let closed = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(
            matches!(closed, Ok(None)),
            "stream did not close on shutdown: {closed:?}"
        );
        task.await.expect("run task panicked");
    }
}
