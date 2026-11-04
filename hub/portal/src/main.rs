//! ProXimos v2 portal binary. See plan/viewer/design.md for architecture.

mod alarms;
mod auth;
mod backfill;
mod beacon_history;
mod calibration;
mod cluster;
mod cluster_radio;
mod config;
mod db;
mod diskspace;
mod display_catalog;
mod display_tz;
mod durable;
mod field_seed;
mod http;
mod i18n;
mod keys;
mod memo;
mod mint_session;
mod monitoring;
mod mqtt;
mod notify;
mod offline;
mod pending;
mod perms;
mod persist;
mod radio;
mod relay;
mod render;
mod retention;
mod routes;
mod sandbox;
mod sensor_picker;
mod sse;
mod state;
mod sync;
mod threshold_defaults;

use std::process::ExitCode;
use std::sync::Arc;

use auth::AuthService;

/// The version this build reports in `/health` and the page footer: the image's release
/// version (`plan/system/design.md` §9.1), which `fiber-portal.bb` passes as
/// `PROXIMOS_VERSION` at compile time on a release-tag build only. Without it, the running
/// image's `VERSION_ID` from `/etc/os-release` (an untagged image, a binary synced onto a
/// Hub), as the agent's System card reads it, and this crate's own version last (a
/// developer's `cargo build` off a Hub).
pub(crate) fn version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| match option_env!("PROXIMOS_VERSION") {
        Some(v) => v.to_string(),
        None => std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|raw| os_release_version(&raw))
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
    })
}

/// `VERSION_ID`, else `VERSION`, from an os-release file; unquoted, `None` when blank.
fn os_release_version(raw: &str) -> Option<String> {
    let field = |name: &str| {
        raw.lines()
            .filter_map(|l| l.trim().strip_prefix(name)?.strip_prefix('='))
            .map(|v| {
                v.trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .trim()
                    .to_string()
            })
            .find(|v| !v.is_empty())
    };
    field("VERSION_ID").or_else(|| field("VERSION"))
}

/// System paths the portal is allowed to touch but does not own. Constants
/// rather than config values on purpose: `plan/security/landlock.md` §2 bans
/// rules assembled from configuration ("no runtime path discovery"). All three
/// are skip-if-absent — a dev host has none of them, and a half-provisioned
/// device should lose a feature rather than fail to confine.
const TLS_DIR: &str = "/data/tls";
const RUNTIME_CONFIG_DIR: &str = "/run/proximos/portal";
const CONTROL_SOCKET: &str = "/run/fiber/control.sock";

/// What the in-process `control.radio` seam touches (`plan/portal/monolith.md`
/// §15): the directory of the rendered device config it loads `radio:` from,
/// the radio state tree, and the ChirpStack gRPC port. Also skip-if-absent: a
/// Hub with no radio loses the seam, not its confinement. The tests at the
/// bottom pin all three to `control-radio`'s own constants, so a move there
/// cannot silently leave the ruleset pointing at the old place.
const RADIO_CONFIG_DIR: &str = "/run/proximos/control";
const RADIO_DATA_DIR: &str = control_radio::store::DEFAULT_ROOT;
const CHIRPSTACK_API_PORT: u16 = 8080;

/// How long a graceful shutdown may take before the process exits regardless.
/// Comfortably under the unit's `TimeoutStopSec`, so systemd never has to
/// SIGKILL us — a SIGKILL skips closing the SQLite connections.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

fn main() -> ExitCode {
    // Ordering here is load-bearing, not stylistic — see
    // plan/security/landlock.md §4. Landlock restricts the *calling thread*,
    // so it must be applied while the process is still single-threaded:
    //
    //   1. logging   — plain `fmt` subscriber, writes synchronously to stderr
    //                  and starts no appender thread (a `non_blocking` writer
    //                  would, and would have to move below the ruleset). It
    //                  comes first so a degraded sandbox is *reportable*.
    //   2. config    — sync read; the ruleset needs its paths.
    //   3. landlock  — the last thing before threads exist.
    //   4. runtime   — explicit Builder, never `#[tokio::main]`, so this
    //                  ordering is visible instead of hidden in a macro.
    //
    // `info` when RUST_LOG is unset, rather than the empty filter
    // `from_default_env()` yields — an empty filter emits *nothing*, not even
    // `error!`, so a failure to bind, read config or confine the process left
    // the binary exiting silently and journald with nothing to show. That is
    // also a direct conflict with plan/security/landlock.md §8 / CP-2, which
    // require degraded confinement to be visible: without a default level we
    // would have the reporting posture of a confined device and the security
    // posture of an unconfined one. RUST_LOG still overrides.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = match config::load() {
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::error!(%err, "failed to load configuration");
            return ExitCode::FAILURE;
        }
    };

    // `mint-session` / `revoke-session` (plan/portal/design.md, amendment 2026-09-25): a local
    // command against the same database, run before the server's sandbox and runtime exist.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = mint_session::dispatch(&args, &cfg.data_dir, cfg.db_key.as_deref()) {
        return code;
    }

    // The data directory must exist before the ruleset opens a handle to it
    // (§4 permits filesystem work before the restriction; it is thread work
    // that is forbidden). On a device `tmpfiles.d` has already created it with
    // the right owner — this is the dev-host fallback, and a failure here is
    // fatal either way since every storage open would fail next.
    if let Err(err) = std::fs::create_dir_all(&cfg.data_dir) {
        tracing::error!(%err, dir = %cfg.data_dir, "failed to create data dir");
        return ExitCode::FAILURE;
    }

    // SQLite's scratch directory, pointed inside our own data directory.
    //
    // Two reasons, and the first is the load-bearing one:
    //
    //  1. `bundled-sqlcipher` compiles with `-DSQLITE_TEMP_STORE=2`, so a temp
    //     database defaults to *memory*. A full VACUUM would then allocate
    //     roughly the whole database in RAM — hundreds of megabytes on a
    //     populated Hub — competing with the agent's 50 ms LED and 250 ms
    //     display threads. The retention connection asks for `temp_store=FILE`
    //     to avoid that, and this is where the file then lands.
    //  2. The default candidate list (`/var/tmp`, `/tmp`, …) is outside the
    //     Landlock ruleset, and `PrivateTmp=true` makes both of those a tmpfs
    //     anyway — i.e. still RAM. A path under `data_dir` is already covered
    //     by the existing recursive rule, so the ruleset and the CP-8 table in
    //     plan/portal/design.md §5 do not change.
    //
    // Set here rather than in the unit so there is one source of truth, and
    // before the ruleset is built because the directory has to exist first.
    let sqlite_tmpdir = std::path::Path::new(&cfg.data_dir).join("tmp");
    match std::fs::create_dir_all(&sqlite_tmpdir) {
        Ok(()) => {
            // SAFETY (edition 2021: safe, but the reasoning matters anyway):
            // the process is still single-threaded here — this is above the
            // runtime for the same reason the ruleset is — so there is no
            // concurrent reader of the environment.
            std::env::set_var("SQLITE_TMPDIR", &sqlite_tmpdir);
        }
        Err(err) => tracing::warn!(
            %err,
            dir = %sqlite_tmpdir.display(),
            "could not create the SQLite temp dir; a full VACUUM would fall back to RAM"
        ),
    }

    let sandbox_status = sandbox::apply(&sandbox::SandboxPaths {
        data_dir: cfg.data_dir.clone(),
        runtime_config_dir: Some(RUNTIME_CONFIG_DIR.to_string()),
        tls_dir: Some(TLS_DIR.to_string()),
        control_socket: Some(CONTROL_SOCKET.to_string()),
        radio_config_dir: Some(RADIO_CONFIG_DIR.to_string()),
        radio_data_dir: Some(RADIO_DATA_DIR.to_string()),
        templates_dir: cfg.templates_dir.clone(),
        static_dir: cfg.static_dir.clone(),
        bind_port: bind_port(&cfg.bind_addr),
        mqtt_port: cfg.mqtt_port,
        chirpstack_api_port: Some(CHIRPSTACK_API_PORT),
    });
    sandbox_status.report();

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            tracing::error!(%err, "failed to build tokio runtime");
            return ExitCode::FAILURE;
        }
    };

    rt.block_on(run(cfg, Arc::new(sandbox_status)))
}

/// Port out of a `host:port` bind address, defaulting to 8088 when it can't
/// be parsed — the bind itself fails informatively later, and the sandbox
/// should not be the thing that reports a malformed address.
fn bind_port(bind_addr: &str) -> u16 {
    bind_addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8088)
}

async fn run(cfg: config::PortalConfig, sandbox_status: Arc<sandbox::SandboxStatus>) -> ExitCode {
    // The port first (C36-10, local smoke test of combined36): a second instance with the
    // same config used to connect to MQTT under the same client id, knocking the running
    // one's session off, open the spill journal and start the notifier before its bind
    // failed. Now it fails before it has touched anything.
    let listener = match tokio::net::TcpListener::bind(&cfg.bind_addr).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!(%err, addr = %cfg.bind_addr, "failed to bind");
            return ExitCode::FAILURE;
        }
    };

    let store = Arc::new(state::StateStore::new());

    // The default bands a newly registered sensor gets (plan/portal/threshold_defaults.md),
    // before the persist thread or a route can register one.
    threshold_defaults::set_configured(cfg.thresholds);

    // `portal.spill.db`, shared by every background writer (`durable`). A full
    // VACUUM locks all eight `portal.db` connections out for its duration; this
    // is where their writes go instead of being dropped, and it is opened
    // before any of them because the persist thread replays whatever a
    // process that died mid-rewrite left behind.
    //
    // Hard-fail like the persist thread it belongs to: without the journal the
    // no-data-loss guarantee (plan/portal/sqlite-vacuum.md §3) does not hold,
    // and on this device that is not a degraded mode worth serving.
    if let Err(err) = std::fs::create_dir_all(&cfg.data_dir) {
        tracing::error!(%err, dir = %cfg.data_dir, "failed to create the data directory");
        return ExitCode::FAILURE;
    }
    let spill = match portal_storage::SpillGate::open(
        std::path::Path::new(&cfg.data_dir),
        cfg.db_key.as_deref(),
    ) {
        Ok(gate) => Arc::new(gate),
        Err(err) => {
            tracing::error!(%err, "failed to open the spill journal");
            return ExitCode::FAILURE;
        }
    };

    // Built up front (M6-E): the alarm notifier and `AlarmService`'s ack
    // side effect (`silence_buzzer`) both need a publish handle, so the
    // MQTT client is constructed before the storage-backed services that
    // depend on it, same as the pre-M6-E `backfill`/`relay` ordering.
    let (mqtt_handle, ingest_task) = mqtt::connect(mqtt::MqttSettings::from(&cfg));

    // The relay and the key store move up here (plan/portal/mqtt/design.md
    // §5.4, §7): the alarm ack's `silence_buzzer` and backfill's
    // `get_sticker_history` are signed commands through the one chokepoint
    // now, not raw publishes, so both services need a relay and a signer at
    // construction rather than an `MqttHandle`.
    let (relay, relay_tx) =
        relay::CommandRelay::spawn(mqtt_handle.clone(), cfg.topic_prefix.clone());

    let keys = match keys::KeyService::open(&cfg.data_dir, cfg.db_key.as_deref()) {
        Ok(keys) => keys,
        Err(err) => {
            tracing::error!(%err, "failed to open key storage");
            return ExitCode::FAILURE;
        }
    };

    // Own `Storage` connection #6 on the same data_dir/db_key — the
    // notifier task (email/webhook/system-alert delivery + history rows).
    // The notifier's live Hub fault verdicts, read by the Hubs page through
    // `AlarmService` (devices.hub.md §2.1).
    let hub_faults = notify::HubFaults::default();
    let notify_tx = match alarms::spawn_notifier(
        &cfg.data_dir,
        cfg.db_key.as_deref(),
        Arc::clone(&spill),
        hub_faults.clone(),
        Arc::clone(&store),
    ) {
        Ok(tx) => tx,
        Err(err) => {
            tracing::error!(%err, "failed to open notifier storage");
            return ExitCode::FAILURE;
        }
    };

    // Own `Storage` connection #7 on the same data_dir/db_key — the alarm
    // read/ack/note API (`routes::` merges this into the router) plus the
    // SSE nudge broadcast the persist thread also holds.
    let alarms = match alarms::AlarmService::open(
        &cfg.data_dir,
        cfg.db_key.as_deref(),
        Arc::clone(&spill),
    ) {
        Ok(alarms) => alarms
            .wire(relay.clone(), keys.clone(), notify_tx.clone())
            .with_hub_faults(hub_faults),
        Err(err) => {
            tracing::error!(%err, "failed to open alarm storage");
            return ExitCode::FAILURE;
        }
    };

    // Cloned before `persist::spawn` consumes the original: the offline
    // sweep (B2) delivers through the same notifier task, and so does the
    // calibration reminder.
    let offline_notify_tx = notify_tx.clone();
    // And the draft's "configuration not delivered" warning (PF-3).
    let pending_notify_tx = notify_tx.clone();

    // The Beacon archive History tab's repaint signal: the persist thread sends
    // on it after an `eye/history` write, the job service after its own.
    let beacon_history_nudge = beacon_history::nudge_channel();
    let calibration_notify_tx = notify_tx.clone();
    let sync_notify_tx = notify_tx.clone();

    let persist_tx = match persist::spawn(
        &cfg.data_dir,
        cfg.db_key.as_deref(),
        cfg.topic_prefix.clone(),
        Arc::clone(&spill),
        notify_tx,
        alarms.nudge_sender(),
        // The canonical Hub spelling for v2 radio rows — `persist::HostnameCanon`.
        Some({
            let store = Arc::clone(&store);
            Arc::new(move |hostname: &str| store.display_hostname(hostname))
                as persist::HostnameCanon
        }),
        // A v2 Node's graded field states, back into the store (P-9 / N32).
        Some({
            let store = Arc::clone(&store);
            Arc::new(
                move |hostname: &str,
                      node_key: &str,
                      states: &std::collections::BTreeMap<String, String>| {
                    store.set_node_field_states(hostname, node_key, states);
                },
            ) as persist::FieldStatesSink
        }),
        Some(beacon_history_nudge.clone()),
        // "Portal storage low" is this Hub's, not every Hub's.
        Some(http::resolve_local_hostname(&cfg)),
    ) {
        Ok(tx) => Some(tx),
        Err(err) => {
            tracing::error!(%err, "failed to open storage");
            return ExitCode::FAILURE;
        }
    };

    // Own `Storage` connection on the same data_dir/db_key (a second
    // SQLite connection under WAL is fine — see `auth::AuthService::open`).
    // Hard-fail like the persistence store above: auth is not optional.
    let auth = match AuthService::open(&cfg.data_dir, cfg.db_key.as_deref(), Arc::clone(&spill)) {
        Ok(auth) => auth,
        Err(err) => {
            tracing::error!(%err, "failed to open auth storage");
            return ExitCode::FAILURE;
        }
    };

    // Own `Storage` connection #3 on the same data_dir/db_key: the request
    // path's handle (`http::AppState::db`).
    let db = match db::Db::open(&cfg.data_dir, cfg.db_key.as_deref(), "request") {
        Ok(db) => db,
        Err(err) => {
            tracing::error!(%err, "failed to open request-path storage");
            return ExitCode::FAILURE;
        }
    };

    // The display zone (plan/decisions.md #25) before the router serves a
    // page or the MQTT loop starts feeding the notifier — until then every
    // instant renders in UTC. A stored name the bundled database no longer
    // knows keeps UTC and says why.
    match db.run(|s| s.display_timezone()).await {
        Ok(zone) if display_tz::set(&zone) => tracing::info!(zone, "display timezone"),
        Ok(zone) => tracing::warn!(zone, "unknown display timezone; showing UTC"),
        Err(err) => tracing::warn!(%err, "display timezone unreadable; showing UTC"),
    }

    // Own `Storage` connection #4 on the same data_dir/db_key — the device
    // CA seed + per-user signing key custody layer (M4-B/M4-D). Hard-fail
    // like the other storage opens above: signed commands are the point of
    // M4, not an optional feature.
    // Own `Storage` connection #5 on the same data_dir/db_key — the
    // history-backfill service (M5-F): job bookkeeping, queue drain,
    // reconciler, gap backfill on a Node's recovery. Hard-fail like the
    // other storage opens above. The notifier is attached afterwards
    // (`with_notifier`) because B3's "cannot download history" alert
    // (devices.backend.md §6.3) rides the same channel B2's offline sweep
    // uses, and that channel is created above both of them.
    let backfill = match backfill::BackfillService::open(
        &cfg.data_dir,
        cfg.db_key.as_deref(),
        relay.clone(),
        backfill::BackfillConfig {
            auto_max_per_10min: cfg.nodes.history_auto_requests_per_10min,
            ..backfill::BackfillConfig::default()
        },
        Arc::clone(&spill),
    ) {
        Ok(backfill) => backfill,
        Err(err) => {
            tracing::error!(%err, "failed to open backfill storage");
            return ExitCode::FAILURE;
        }
    };
    // History requests on a v2 Hub go through the in-process radio seam, not the relay
    // (p2p_history_replay.md §14). One seam for the process: it reads the rendered radio
    // config once, and the request path gets the same instance through `http::app`.
    let radio = radio::RadioControl::from_env();
    let backfill = backfill
        .with_notifier(offline_notify_tx.clone())
        .with_keys(keys.clone())
        .with_radio(Arc::new(radio.clone()));

    // Per-device offline detection (B2, devices.backend.md §5). Shares the
    // request path's connection rather than opening another: the sweep does
    // two small bulk reads a minute.
    // Not fatal in any path — it owns no storage of its own to fail on.
    //
    // Constructed **before** `backfill.spawn_tasks`: B3's auto-backfill
    // trigger is a subscription to this service's transition broadcast
    // (devices.backend.md §6.1/§16.1), and a `broadcast` receiver only sees
    // what is sent after it exists. The two orderings differ by exactly the
    // first sweep, which is the one that classifies the whole fleet.
    let offline = offline::OfflineService::new(
        db.clone(),
        cfg.nodes.clone(),
        cfg.beacons.clone(),
        offline_notify_tx,
    );
    backfill.spawn_tasks(Arc::clone(&store), Some(offline.subscribe_transitions()));
    // Beacon archive history (plan/portal/beacon_history.md §4): its own
    // connection, the automatic request on a Beacon's return and the
    // 30-minute silence reaper. Subscribed before the first sweep, as above.
    let beacon_history = match beacon_history::BeaconHistoryService::open(
        &cfg.data_dir,
        cfg.db_key.as_deref(),
        relay.clone(),
        Arc::clone(&spill),
        cfg.beacons.clone(),
        beacon_history_nudge,
    ) {
        Ok(service) => service.with_keys(keys.clone()),
        Err(err) => {
            tracing::error!(%err, "failed to open beacon history storage");
            return ExitCode::FAILURE;
        }
    };
    beacon_history.spawn_tasks(Arc::clone(&store), Some(offline.subscribe_transitions()));
    // MB3's liveness writer (monitoring.backend.md §6 WP3.2): the same
    // transition broadcast, turned into one `alarm_events` row per outage so
    // the probe-detail window's event list and the chart's marklines read
    // from the one source every other alarm already uses. Subscribed here,
    // **before** `spawn_task`, for the reason the comment above gives: a
    // `broadcast` receiver only sees what is sent after it exists, and the
    // first sweep is the one that classifies the whole fleet.
    alarms.spawn_liveness_writer(offline.subscribe_transitions(), Arc::clone(&store));
    // Orphaned alarms (monitoring.md, amendment 2026-10-14): first pass after
    // the registry has refilled from retained messages, then periodically.
    alarms.spawn_orphan_sweeper(
        Arc::clone(&store),
        std::time::Duration::from_secs(300),
        std::time::Duration::from_secs(900),
    );
    offline.spawn_task(Arc::clone(&store));

    // Calibration certificates and their hourly reminder (calibration.cs.md
    // §3). The request path's connection, like the offline sweep: one bulk
    // read an hour and a few alarm writes. Its alarm rows nudge the same
    // SSE alarm table the persist thread's do, and its onset/escalation
    // e-mails ride the notifier the offline sweep uses. Not fatal in any
    // path — it owns no storage of its own to fail on.
    calibration::set_label_second_warning_days(cfg.calibration.second_warning_days);
    let calibration = calibration::CalibrationService::new(
        db.clone(),
        cfg.calibration,
        &cfg.data_dir,
        Arc::clone(&store),
    )
    .wire(calibration_notify_tx, alarms.nudge_sender());
    calibration.spawn_task();

    // B4's Node configuration draft (devices.backend.md §7 WP4.2). Its own
    // connection, like `backfill`'s: the send walk is a detached task that
    // lives for minutes and writes repeatedly over that span, and those
    // writes have no business queueing with the request path's reads. The
    // lock itself is never held across an await — it is taken for one
    // statement at a time, on the blocking pool except for
    // `mark_interrupted_blocking`, which takes it on a runtime thread and
    // says there why that is safe.
    //
    // **Fatal if it fails to open**, unlike retention: without it the two
    // configuration routes have nowhere to write and would silently accept
    // edits that go nowhere. Opening also clears every `sending` row left
    // by a process that died mid-walk — nothing in the new process is
    // waiting for those devices, so the state is unrecoverable rather than
    // stale (see `PendingService::open`).
    let pending = match pending::PendingService::open(&cfg.data_dir, cfg.db_key.as_deref()) {
        Ok(pending) => pending.wire(Arc::clone(&store), pending_notify_tx, alarms.nudge_sender()),
        Err(err) => {
            tracing::error!(%err, "failed to open the pending-config store");
            return ExitCode::FAILURE;
        }
    };
    // P-6: a failed draft row the Node later reports as its config was applied late.
    pending.spawn_late_apply(Arc::clone(&store));
    // PF-3: a Node unit whose ack came after the walk gave up on it is finished then, and the
    // warnings of Nodes a restart left undelivered are opened first.
    pending.spawn_late_answers(Arc::clone(&store));
    // config_sync.md §13 Q10: a Node that has not confirmed its configuration 4 h after the
    // last send gets its `cfgsync:` device fault.
    pending.spawn_deadline_sweep();

    // Last `Storage` connection — the retention sweep (M7). Not fatal if it
    // fails to open: falling behind on housekeeping degrades disk headroom,
    // it does not stop monitoring (same reasoning as the sandbox's fail-open).
    // The maintenance handle rides on the retention task's connection (see
    // `RetentionTask::maintenance`), so it is taken before the task is consumed
    // by `spawn`. `None` when retention failed to open: the settings page then
    // reports maintenance as unavailable rather than the process refusing to
    // serve, same fail-soft stance as the sweep itself.
    let maintenance = match retention::RetentionTask::open(
        &cfg.data_dir,
        cfg.db_key.as_deref(),
        portal_storage::RetentionPolicy::default(),
        Arc::clone(&spill),
        persist_tx.clone(),
    ) {
        Ok(task) => {
            let task = task.with_maintenance_window(cfg.maintenance_window);
            let maintenance = task.maintenance();
            task.spawn();
            Some(maintenance)
        }
        Err(err) => {
            tracing::error!(%err, "retention task not started; /data will grow unbounded");
            None
        }
    };

    // GitLab #61: `pair/invite` — a device on this machine asking the portal to pair with it,
    // because the operator is standing at the Hub with a phone and no LAN. Its own sink rather
    // than the relay's: pairing *starts* here and needs the key store and the audit log.
    let (pair_invite_tx, pair_invite_rx) = tokio::sync::mpsc::unbounded_channel();

    // GitLab #120: switched-off sensors, before the ingest fills the store, so their values
    // are parked from the first one on. A failure is logged: every sensor then shows as on,
    // and storage still drops what it must not keep.
    match db.run(|s| s.list_disabled_sensors()).await {
        Ok(rows) => store.load_disabled_sensors(
            rows.into_iter()
                .map(|r| (r.hostname, r.scope, r.entity, r.field)),
        ),
        Err(err) => tracing::warn!(%err, "switched-off sensors not loaded"),
    }

    // Decision #41: the Portal's wired-line bounds, before the ingest, so the first
    // `config/state` echo is already shown under them.
    match db
        .run(|s| s.list_field_thresholds_in_scope(portal_storage::THRESHOLD_SCOPE_HUB))
        .await
    {
        Ok(rows) => {
            let defaults = threshold_defaults::configured();
            // A switched-off band (#167) is loaded too: its bounds stay shown, marked off.
            store.load_line_limits(rows.into_iter().filter_map(|r| {
                let line = r.entity.parse::<u8>().ok()?;
                let h = threshold_defaults::hysteresis(
                    &defaults,
                    portal_storage::THRESHOLD_SCOPE_HUB,
                    &r.field,
                    r.hysteresis,
                );
                let bounds = [
                    r.critical_low,
                    r.warning_low,
                    r.warning_high,
                    r.critical_high,
                ];
                Some((
                    r.hostname,
                    line,
                    routes::hub_routes::line_limits(bounds, h, r.enabled),
                ))
            }))
        }
        Err(err) => tracing::warn!(%err, "Portal line limits not loaded"),
    }

    // Decision #41: the Portal's Node and Beacon names and locations, before the ingest, so
    // the first inventory echo is already shown under them.
    match db.run(|s| s.list_device_meta_labels()).await {
        Ok(rows) => store.load_meta_labels(
            rows.into_iter()
                .map(|r| (r.hostname, r.scope, r.entity, r.name, r.location)),
        ),
        Err(err) => tracing::warn!(%err, "Portal device names not loaded"),
    }

    // Before the ingest: the retained inventory replayed on connect is what it reacts to (N14).
    field_seed::spawn(Arc::clone(&store), db.clone());

    // Config sync, Portal side of the Hub pull (config_sync.md §5, decision #43).
    let (sync_tx, sync_rx) = tokio::sync::mpsc::unbounded_channel();
    sync::SyncService::new(
        db.clone(),
        keys.clone(),
        relay.clone(),
        auth.clone(),
        Arc::clone(&store),
        Some(alarms.nudge_sender()),
    )
    .with_notify(sync_notify_tx)
    .spawn(sync_rx);
    let ingest_task = ingest_task.with_sync(sync_tx);

    tokio::spawn(ingest_task.run(
        Arc::clone(&store),
        persist_tx,
        Some(relay_tx),
        Some(pair_invite_tx),
    ));

    // Watch rather than broadcast: late subscribers (a tab that connects
    // mid-shutdown) must see the current value, not miss a one-shot event.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let app = http::app(
        &cfg,
        store,
        auth,
        db,
        Arc::clone(&spill),
        relay,
        radio,
        keys,
        backfill,
        beacon_history,
        alarms,
        pending,
        offline,
        calibration,
        maintenance,
        sandbox_status,
        shutdown_rx,
        Some(pair_invite_rx),
    );

    tracing::info!(addr = %cfg.bind_addr, "portal listening");

    if let Err(err) = http::serve(
        listener,
        app,
        shutdown_tx,
        SHUTDOWN_GRACE,
        http::shutdown_signal(),
    )
    .await
    {
        tracing::error!(%err, "server error");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::{os_release_version, CHIRPSTACK_API_PORT, RADIO_CONFIG_DIR, RADIO_DATA_DIR};

    /// The footer shows the image's version on an untagged build (bench test 2026-10-29).
    #[test]
    fn the_version_comes_from_os_release() {
        let raw = "NAME=\"ProXimos\"\nVERSION=\"v1 (rc)\"\nVERSION_ID=v1.0.0-rc1-2-gabc\n";
        assert_eq!(
            os_release_version(raw).as_deref(),
            Some("v1.0.0-rc1-2-gabc")
        );
        assert_eq!(
            os_release_version("VERSION=\"v2\"\n").as_deref(),
            Some("v2")
        );
        assert_eq!(os_release_version("VERSION_ID=\nNAME=x\n"), None);
    }

    /// The ruleset names the directory; control-radio names the file inside it.
    #[test]
    fn the_radio_config_dir_holds_the_file_the_seam_loads() {
        let file = control_radio::dispatch::DEFAULT_CONFIG_PATH;
        assert_eq!(
            std::path::Path::new(file).parent(),
            Some(std::path::Path::new(RADIO_CONFIG_DIR)),
            "{file} moved out of the directory the Landlock ruleset admits"
        );
    }

    #[test]
    fn the_radio_data_dir_is_the_store_root() {
        assert_eq!(RADIO_DATA_DIR, control_radio::store::DEFAULT_ROOT);
    }

    /// `API_ENDPOINT` is a URL, deliberately not configurable; the port in it is
    /// the one the ruleset has to let the seam connect to.
    #[test]
    fn the_chirpstack_port_is_the_api_endpoints() {
        let endpoint = control_radio::lorawan::client::API_ENDPOINT;
        let port = endpoint
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok());
        assert_eq!(
            port,
            Some(CHIRPSTACK_API_PORT),
            "{endpoint} is not what the ruleset admits"
        );
    }
}
