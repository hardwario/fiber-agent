//! `proximos-control` — the resident half of the pair.
//!
//! Decision #12 dissolved the radio daemon and its second control socket, and moved its three
//! loop duties here (`plan/control/radio/plan.md`, "Runtime form"): the ChirpStack event
//! subscription that feeds the §7 decode path, the 60 s `radio/gateways` health poll, and the
//! `node-get` cache. `plan/portal/monolith.md` §9 makes this same process the host the rest of
//! the agent moves into, which is why it stays alive under `radio.mode: off` instead of
//! exiting: the unit's existence is not a statement about the radio.
//!
//! All of the logic lives in `control_radio`. This binary is the loop, the sockets and the
//! supervision — deliberately, because decision #12's standalone guarantee only means anything
//! if the crate can still be driven with none of this present, which is what
//! `control-radio-harness` and the unit tests do.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use control_radio::config::RadioConfig;
use control_radio::mode::Mode;

mod broker;
mod client_page;
mod p2p_task;
mod supervise;

/// A plain `fn main()`, deliberately **not** `#[tokio::main]`.
///
/// `security/landlock.md` §4 is binding: `landlock_restrict_self` is per-thread, so the ruleset
/// must be applied while the process is still single-threaded. `#[tokio::main]` expands to a
/// runtime built before the first line of the async body runs, which would leave every worker
/// thread unconfined — silently, with every functional test still passing. So confinement goes
/// first and the runtime is built by hand afterwards, exactly as in `proximosctl`.
///
/// Phase 1 leaves the call site empty (CP-3 is pending spike S3), but the shape is
/// load-bearing: filling it in must not require restructuring `main`.
fn main() -> ExitCode {
    confine();

    // Built by hand after confinement — never `#[tokio::main]`, per the note above. Two
    // workers: the work is one MQTT loop, one 60 s timer and a decode that is microseconds of
    // arithmetic per frame. A resident process on a Hub should not hold a thread per core for
    // that.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("proximos-control: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match rt.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("proximos-control: {e}");
            // The §6 taxonomy's exit code, not an ad-hoc number: `Restart=on-failure` and any
            // operator reading `systemctl status` see the same contract the CLI publishes.
            u8::try_from(e.exit_code())
                .map(ExitCode::from)
                .unwrap_or(ExitCode::FAILURE)
        }
    }
}

/// Apply the Landlock ruleset. Empty in phase 1 — see the note on [`main`].
///
/// The rows this process owes `security/landlock.md` §5.1 when the call site is filled in
/// (`plan.md` §Sandboxing, plus this binary's own runtime state):
///
/// | Path | Rights | What for |
/// |---|---|---|
/// | `/data/proximos/radio/` | read, write, create, remove, truncate | the component state tree — `lorawan/config.json`, `nodes.json`, the token files, and the `node-get` cache the uplink task fills |
/// | `/run/proximos/control/config.yaml` | read | the rendered device config, and nothing else in the renderer's directory |
/// | `/run/proximos/radio/` | read, write, create, truncate | this component's own runtime directory — the `alive` liveness file `node-get` reads |
/// | `/dev/ttyAMA3` | read, write, ioctl | the northbridge UART — the P2P central is hosted here since PI1 (`p2p_task`); its `sessions.db` sits under `/data/proximos/radio/p2p/`, inside the first row |
///
/// ⚠️ `security/landlock.md` §5.1 carries none of these today. Phase 1 leaves the call site
/// empty so nothing breaks at runtime, but the rows are owed — the same registration discipline
/// `plan.md` §Sandboxing already flags for the crate.
fn confine() {}

/// Load the config, then either idle or run the radio tasks until SIGTERM.
async fn run() -> control_radio::Result<()> {
    // `PROXIMOS_CONFIG`, defaulting to `/run/proximos/control/config.yaml` — the renderer's
    // output. Resolved by `control_radio::dispatch` so the daemon and the CLI can never
    // disagree about which file is the device config.
    let config = control_radio::dispatch::load_config()?;
    let shutdown = supervise::Shutdown::new();

    // SIGTERM is what systemd sends on `stop` and on the restart half of a redeploy; SIGINT is
    // what an engineer running this in a terminal sends. Both mean the same thing here.
    let signals = shutdown.clone();
    tokio::spawn(async move { signals.wait_for_signal().await });

    // Created before the `off` branch: the Client's info page (decisions.md #46) reads it in
    // every mode, and an `off` Hub's document simply stays `disabled`.
    let nb_status = control_radio::p2p::northbridge::NorthbridgeStatus::default();
    if !p2p_task::enabled(&config) {
        nb_status.disabled();
    }
    // The Client's info page, in every mode. A Server clears a page a previous role left.
    let membership = p2p_task::gateway_role().map(client_page::Membership::from);
    match membership {
        None => client_page::clear(std::path::Path::new(client_page::DIR)),
        Some(membership) => {
            let (mode, nb_status) = (config.mode, nb_status.clone());
            supervise::spawn("client-page", shutdown.clone(), {
                let shutdown = shutdown.clone();
                move || {
                    let (membership, nb_status, shutdown) =
                        (membership.clone(), nb_status.clone(), shutdown.clone());
                    async move {
                        client_page::run(
                            client_page::DIR.into(),
                            membership,
                            mode,
                            nb_status,
                            shutdown,
                        )
                        .await
                    }
                }
            });
        }
    }

    if config.mode == Mode::Off {
        // **Alive, not exited.** Under `radio.mode: off` there is no radio to serve, but
        // monolith.md §9 makes this unit the agent's host — the portal ingest, the BLE control
        // surface and the rest arrive in this same process. A daemon that exited here would
        // have to be re-enabled by hand the day one of them lands, and `Restart=on-failure`
        // plus a clean exit would leave the unit `inactive (dead)` on a Hub whose operator has
        // no reason to think anything is wrong.
        //
        // The heartbeat is deliberately **not** touched: it vouches for the radio tasks, and
        // `node-get` on an `off` Hub is refused by the mode gate long before it reads a cache.
        eprintln!(
            "proximos-control: radio.mode is off — no radio tasks; staying alive as the \
             agent host (portal/monolith.md §9)"
        );
        shutdown.wait().await;
        return Ok(());
    }

    let ctx = Arc::new(Context::new(config));
    let observed = Arc::new(control_radio::mqtt::ObservedRetainedTopics::new());

    // The P2P central (PI1, `plan/portal/p2p_integration.md` §4.1), when `radio.mode` allows
    // the P2P transport. Its broker client and its liveness handle are created once, here, and
    // outlive the supervisor's restarts of the task (see `p2p_task`).
    // The northbridge status document (`northbridge_diagnostics.md` §3), published by the
    // health loop whether or not this Hub runs P2P — `disabled` says so rather than leaving a
    // stale retained document from the last mode claiming a radio that is off.
    // A cluster Client lends its Northbridge to the Server's central instead of running one
    // (`p2p_cluster_gateway.md` §3.2): no publisher, no outbox, no central liveness here.
    let gateway = p2p_task::enabled(&ctx.config)
        .then(p2p_task::gateway_role)
        .flatten();
    let p2p = (p2p_task::enabled(&ctx.config) && gateway.is_none()).then(|| {
        let publisher = Arc::new(control_radio::p2p::publisher::RumqttcPublisher::connect_as(
            p2p_task::CLIENT_ID,
            control_radio::northbound::hostname(),
        ));
        // The northbound outbox and its one drainer, for the life of the process like the
        // broker client: a restart of the central must not abandon a publish in flight and
        // have a second drainer send it again (p2p_tower_gateway.md §8.6).
        let outbox = p2p_task::open_outbox();
        tokio::spawn(outbox.clone().run_drainer(publisher.clone()));
        (
            Arc::new(ctx.config.clone()),
            publisher,
            control_radio::p2p::runtime::Liveness::default(),
            outbox,
        )
    });
    let p2p_liveness = p2p.as_ref().map(|(_, _, liveness, _)| liveness.clone());
    // The central's gateways (`p2p_cluster_gateway.md` §8), published by the health loop beside
    // the Northbridge document; kept across the central's restarts like the status handle.
    let gw_board = p2p
        .as_ref()
        .map(|_| control_radio::p2p::gw_board::GatewayBoard::default());
    // A Client that lends its Northbridge leaves the document's topic, on the Server's broker,
    // to the Server's central, which drives it (§8).
    let lends_northbridge = gateway.is_some();

    // Two supervised tasks. Each restarts on its own backoff, because their failure modes are
    // unrelated: the broker loop dies when mosquitto restarts, the health poll fails when
    // ChirpStack is migrating its database on first boot, and neither is a reason to tear the
    // other down. Nothing here exits the process — `Restart=on-failure` is for a config the
    // daemon cannot use at all, not for a dependency that is briefly absent.
    //
    // The heartbeat is not a third task: it rides the health tick, because the file has to
    // vouch for work actually happening rather than for a timer still firing.
    let broker_task = supervise::spawn("broker", shutdown.clone(), {
        let (ctx, observed, shutdown) = (ctx.clone(), observed.clone(), shutdown.clone());
        move || {
            let (ctx, observed, shutdown) = (ctx.clone(), observed.clone(), shutdown.clone());
            async move { broker::run(ctx, observed, shutdown).await }
        }
    });

    let health_task = supervise::spawn("health", shutdown.clone(), {
        let (ctx, observed, shutdown) = (ctx.clone(), observed.clone(), shutdown.clone());
        let p2p_liveness = p2p_liveness.clone();
        let nb_status = nb_status.clone();
        let gw_board = gw_board.clone();
        move || {
            let (ctx, observed, shutdown) = (ctx.clone(), observed.clone(), shutdown.clone());
            let docs = NbDocs {
                liveness: p2p_liveness.clone(),
                status: nb_status.clone(),
                board: gw_board.clone(),
                lends_northbridge,
            };
            async move { health_loop(ctx, observed, docs, shutdown).await }
        }
    });

    let mut handles = vec![broker_task.handle, health_task.handle];
    if let Some((config, publisher, liveness, outbox)) = p2p {
        let p2p_task = supervise::spawn("p2p", shutdown.clone(), {
            let (shutdown, nb_status) = (shutdown.clone(), nb_status.clone());
            let gw_board = gw_board.clone().unwrap_or_default();
            move || {
                let (config, publisher, liveness, outbox, nb_status, gw_board, shutdown) = (
                    config.clone(),
                    publisher.clone(),
                    liveness.clone(),
                    outbox.clone(),
                    nb_status.clone(),
                    gw_board.clone(),
                    shutdown.clone(),
                );
                async move {
                    p2p_task::run(
                        config, publisher, liveness, outbox, nb_status, gw_board, shutdown,
                    )
                    .await
                }
            }
        });
        handles.push(p2p_task.handle);
    }
    // Without P2P nothing else owns the Northbridge's UART: the idle task reads its firmware
    // and answers `northbridge-update` in any mode (#158).
    if !p2p_task::enabled(&ctx.config) {
        let nb_idle = supervise::spawn("northbridge", shutdown.clone(), {
            let (shutdown, nb_status) = (shutdown.clone(), nb_status.clone());
            move || {
                let (nb_status, shutdown) = (nb_status.clone(), shutdown.clone());
                async move { p2p_task::run_idle(nb_status, shutdown).await }
            }
        });
        handles.push(nb_idle.handle);
    }
    if let Some(broker) = gateway {
        let p2p_gateway = supervise::spawn("p2p-gateway", shutdown.clone(), {
            let (shutdown, nb_status) = (shutdown.clone(), nb_status.clone());
            move || {
                let (broker, nb_status, shutdown) =
                    (broker.clone(), nb_status.clone(), shutdown.clone());
                async move { p2p_task::run_gateway(broker, nb_status, shutdown).await }
            }
        });
        handles.push(p2p_gateway.handle);
    }
    for handle in handles {
        let _ = handle.await;
    }
    Ok(())
}

/// Everything the loops need that does not change while the process runs.
pub struct Context {
    pub config: RadioConfig,
    pub store: control_radio::store::Store,
    pub topics: control_radio::northbound::Topics,
    /// The forwarder's topic prefix. Configuration, not a constant (`lorawan.md` §6).
    pub prefix: String,
    /// What the broker task has seen of the concentrator (stats, conn state), for the LoRaWAN
    /// status document (`northbridge_diagnostics.md` §4a).
    pub lrw: control_radio::lorawan::diagnostics::Observed,
}

impl Context {
    fn new(config: RadioConfig) -> Self {
        let prefix = std::env::var("PROXIMOS_FORWARDER_TOPIC_PREFIX")
            .unwrap_or_else(|_| config.region.as_str().to_string());
        Context {
            store: control_radio::store::Store::new(control_radio::dispatch::store_root()),
            topics: control_radio::northbound::Topics::new(control_radio::northbound::hostname()),
            prefix,
            config,
            lrw: control_radio::lorawan::diagnostics::Observed::default(),
        }
    }

    /// The converged application id, or `None` before `create` has run.
    ///
    /// Re-read rather than cached: `radio-setup.service` is ordered before this unit but
    /// `Before=` is ordering, not a dependency (its own comment says so), and on a first boot
    /// where `create` is still retrying against a migrating ChirpStack this process is already
    /// up. Caching an absent id at startup would mean never subscribing at all until a restart.
    pub fn application_id(&self) -> Option<String> {
        let raw = self
            .store
            .read(control_radio::store::Slot::LorawanConfig)
            .ok()??;
        control_radio::lorawan::ConvergedState::from_json(&raw)
            .ok()?
            .application_id
            .filter(|s| !s.is_empty())
    }

    /// The tenant `create` resolved, for the health poll's `list_gateways`.
    pub fn tenant_id(&self) -> Option<String> {
        let raw = self
            .store
            .read(control_radio::store::Slot::LorawanConfig)
            .ok()??;
        control_radio::lorawan::ConvergedState::from_json(&raw)
            .ok()?
            .tenant_id
            .filter(|s| !s.is_empty())
    }
}

/// The 60 s health tick: poll, publish `radio/gateways`, and touch the liveness file.
///
/// The heartbeat rides this tick rather than a timer of its own, for the reason
/// `tasks::heartbeat` states: the file is meant to assert that the daemon is still *doing its
/// job*, and a timer would keep ticking while everything it vouched for was wedged.
async fn health_loop(
    ctx: Arc<Context>,
    observed: Arc<control_radio::mqtt::ObservedRetainedTopics>,
    docs: NbDocs,
    shutdown: supervise::Shutdown,
) -> control_radio::Result<()> {
    let NbDocs {
        liveness: p2p_liveness,
        status: nb_status,
        board,
        lends_northbridge,
    } = docs;
    use control_radio::lorawan::client::ChirpStack;
    use control_radio::northbound::RetainedPublisher;

    let period = control_radio::tasks::health::interval();
    let mut poller = control_radio::tasks::health::HealthPoller::new();
    let publisher = control_radio::mqtt::RumqttcPublisher::new();
    // Whether the last tick withheld the heartbeat, so the log says so once per episode.
    let mut withheld = false;
    // The concentratord and ChirpStack versions, read once per task run — they change only
    // with an image update, which restarts this process anyway.
    let mut versions: Option<(Option<String>, Option<String>)> = None;

    while !shutdown.is_shutting_down() {
        // Connected per tick, not held: the client is a tonic channel with a 5 s connect
        // timeout, and a ChirpStack restart between ticks would otherwise leave a dead channel
        // reporting every gateway unreachable until this task was restarted. A failure is not
        // an error — it is an `api_status`, which is the whole point of §6's split.
        let tokens = control_radio::lorawan::Tokens::new(ctx.store.clone());
        let client = control_radio::lorawan::tonic_client::TonicChirpStack::connect(tokens)
            .await
            .ok();
        let tenant = ctx.tenant_id();

        let report = poller
            .tick(control_radio::tasks::health::Inputs {
                client: client.as_ref().map(|c| c as &dyn ChirpStack),
                tenant_id: tenant.as_deref(),
                topics: Some(observed.as_ref()),
                prefix: &ctx.prefix,
                gateway_name: &ctx.config.gateway.name,
            })
            .await;

        // #156: a gateway the forwarder announced only after `radio-setup` read the retained
        // topics is left `pending` by `create` and never registered — ChirpStack then drops
        // every frame from our own concentrator. Register it here, with `create`'s own step.
        let unregistered = poller.unregistered();
        if !unregistered.is_empty() && ctx.config.mode.allows(control_radio::Transport::Lorawan) {
            if let Some(c) = client.as_ref() {
                let tokens = control_radio::lorawan::Tokens::new(ctx.store.clone());
                let conv = control_radio::lorawan::converge::Converger {
                    client: c,
                    config: &ctx.config,
                    tokens: &tokens,
                    store: &ctx.store,
                };
                match conv.adopt_gateway(&unregistered).await {
                    Ok(Some(eui)) => eprintln!(
                        "proximos-control: registered local gateway {} that create left pending",
                        eui.as_str()
                    ),
                    Ok(None) => {}
                    Err(e) => eprintln!("proximos-control: cannot register local gateway: {e}"),
                }
            }
        }

        match serde_json::to_vec(&report) {
            // `radio/gateways` is retained: it is the current health of the radio, and a late
            // subscriber — the portal reconnecting after an update — needs it immediately
            // rather than up to a minute later. Unlike a `reading`, a stale one is not
            // mistakable for something live: it carries its own `timestamp`.
            Ok(payload) => {
                if let Err(e) = publisher
                    .publish_retained(&[(
                        control_radio::northbound::GATEWAYS_TOPIC.to_string(),
                        payload,
                    )])
                    .await
                {
                    eprintln!("proximos-control: cannot publish radio/gateways: {e}");
                }
            }
            Err(e) => eprintln!("proximos-control: cannot render radio/gateways: {e}"),
        }

        // The LoRaWAN status document (`northbridge_diagnostics.md` §4a), from this tick's
        // API status and a look at the card and the units.
        let api = match report.api_status {
            control_radio::lorawan::gateway::ApiStatus::Ok => "ok",
            control_radio::lorawan::gateway::ApiStatus::AuthFailed => "auth_failed",
            control_radio::lorawan::gateway::ApiStatus::Unreachable => "unreachable",
        };
        publish_lorawan(&publisher, &ctx, api, &mut versions).await;

        // The heartbeat vouches for every radio task this process runs, so it waits for the P2P
        // central too (§4.1): a northbridge that stopped answering must read as `DaemonDown`,
        // not as a live daemon with an empty cache.
        let now = u64::try_from(control_radio::lorawan::gateway::now_unix()).unwrap_or(0);
        if !vouches_for_radio(p2p_liveness.as_ref(), now) {
            if !withheld {
                eprintln!(
                    "proximos-control: no sign of life from the northbridge — withholding the \
                     heartbeat until the P2P central hears from it again"
                );
                withheld = true;
            }
        } else {
            if withheld {
                eprintln!("proximos-control: the northbridge answers again — heartbeat resumed");
                withheld = false;
            }
            touch_heartbeat();
        }

        // The northbridge document every tick, and between ticks whenever it changes, so a
        // fault or a lost link reaches the portal within seconds rather than a minute. One task
        // publishes everything: the retained publisher's client id is per process.
        let mut nb_version = publish_northbridge(
            &publisher,
            &nb_status,
            p2p_liveness.as_ref(),
            !lends_northbridge,
        )
        .await;
        let mut board_seen = match &board {
            Some(b) => Some(publish_gateways(&publisher, b).await),
            None => None,
        };
        let deadline = tokio::time::Instant::now() + period;
        while !shutdown.is_shutting_down() && tokio::time::Instant::now() < deadline {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            shutdown.sleep(left.min(NB_POLL)).await;
            if nb_status.version() != nb_version {
                nb_version = publish_northbridge(
                    &publisher,
                    &nb_status,
                    p2p_liveness.as_ref(),
                    !lends_northbridge,
                )
                .await;
            }
            if let Some(b) = &board {
                if board_seen.as_ref() != Some(&b.fingerprint()) {
                    board_seen = Some(publish_gateways(&publisher, b).await);
                }
            }
        }
    }
    Ok(())
}

/// Gather the §4a inputs and publish the LoRaWAN document retained, mirrored under `/run`.
async fn publish_lorawan(
    publisher: &control_radio::mqtt::RumqttcPublisher,
    ctx: &Context,
    api: &'static str,
    versions: &mut Option<(Option<String>, Option<String>)>,
) {
    use control_radio::lorawan::diagnostics as d;
    use control_radio::northbound::RetainedPublisher;

    let wants = ctx.config.mode.allows(control_radio::Transport::Lorawan);
    let probe_versions = wants && versions.is_none();
    // A cluster Client runs no ChirpStack (cluster.md §2). Its witness is the rendered config
    // (decision #11), the same one the renderer drops ChirpStack on (§4.2, C3).
    // systemctl and sysfs block; off the async workers.
    let gathered = tokio::task::spawn_blocking(move || {
        let card = d::probe_card(
            std::path::Path::new(d::CARD_TTY),
            std::path::Path::new("/sys/class/tty"),
        );
        if !wants {
            return Err(card);
        }
        let units = (
            d::query_unit(d::UNIT_CONCENTRATORD),
            d::query_unit(d::UNIT_FORWARDER),
            d::query_unit(d::UNIT_CHIRPSTACK),
        );
        let found = probe_versions.then(|| {
            (
                d::tool_version("/usr/bin/chirpstack-concentratord-sx1302"),
                d::tool_version("/usr/bin/chirpstack"),
            )
        });
        let uptime = std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|s| s.split(['.', ' ']).next()?.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        let client = p2p_task::gateway_role().is_some();
        Ok((card, units, found, uptime, client))
    })
    .await
    .ok();

    let now = control_radio::lorawan::gateway::now_unix();
    let inputs = match gathered {
        Some(Ok((card, (concentratord, forwarder, chirpstack), found, uptime_s, client))) => {
            if let Some(v) = found {
                *versions = Some(v);
            }
            let (concentratord_version, chirpstack_version) =
                versions.clone().unwrap_or((None, None));
            d::Inputs {
                now,
                uptime_s,
                wants_lorawan: true,
                client,
                card,
                concentratord,
                forwarder,
                chirpstack,
                api: Some(api),
                observed: ctx.lrw.snapshot(),
                concentratord_version,
                chirpstack_version,
            }
        }
        not_wanted => d::Inputs {
            now,
            uptime_s: u64::MAX,
            wants_lorawan: false,
            client: false,
            card: match not_wanted {
                Some(Err(card)) => card,
                _ => d::Card {
                    present: false,
                    device: d::CARD_TTY.into(),
                    usb_id: None,
                },
            },
            concentratord: None,
            forwarder: None,
            chirpstack: None,
            api: None,
            observed: Default::default(),
            concentratord_version: None,
            chirpstack_version: None,
        },
    };
    let payload = match serde_json::to_vec(&d::document(&inputs)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("proximos-control: cannot render the LoRaWAN document: {e}");
            return;
        }
    };
    let topic = d::topic(&control_radio::northbound::hostname());
    if let Err(e) = publisher
        .publish_retained(&[(topic, payload.clone())])
        .await
    {
        eprintln!("proximos-control: cannot publish the LoRaWAN document: {e}");
    }
    write_run_file("lorawan.json", &payload);
}

/// How often the health loop looks for a changed northbridge status between ticks.
const NB_POLL: Duration = Duration::from_secs(2);

/// The documents the health loop publishes about the P2P radio.
struct NbDocs {
    /// The central's liveness, when this Hub runs one.
    liveness: Option<control_radio::p2p::runtime::Liveness>,
    /// This Hub's Northbridge document.
    status: control_radio::p2p::northbridge::NorthbridgeStatus,
    /// The central's gateways, when this Hub runs one.
    board: Option<control_radio::p2p::gw_board::GatewayBoard>,
    /// A Client lending its Northbridge: the Server's central publishes its document.
    lends_northbridge: bool,
}

/// Publish the northbridge document retained (unless `mqtt` is off: a Client that lends its
/// Northbridge) and mirror it under `/run` for the CLI; returns the status version it
/// published.
async fn publish_northbridge(
    publisher: &control_radio::mqtt::RumqttcPublisher,
    status: &control_radio::p2p::northbridge::NorthbridgeStatus,
    liveness: Option<&control_radio::p2p::runtime::Liveness>,
    mqtt: bool,
) -> u64 {
    use control_radio::northbound::RetainedPublisher;
    let version = status.version();
    let now = control_radio::lorawan::gateway::now_unix();
    let alive = liveness.is_some_and(|l| p2p_task::is_alive(l, u64::try_from(now).unwrap_or(0)));
    let doc = status.document(now, alive);
    let payload = match serde_json::to_vec(&doc) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("proximos-control: cannot render the northbridge document: {e}");
            return version;
        }
    };
    let topic = control_radio::p2p::northbridge::topic(&control_radio::northbound::hostname());
    if mqtt {
        if let Err(e) = publisher
            .publish_retained(&[(topic, payload.clone())])
            .await
        {
            eprintln!("proximos-control: cannot publish the northbridge document: {e}");
        }
    }
    write_run_file("northbridge.json", &payload);
    version
}

/// Publish, retained, the remote gateways' Northbridge documents and the `p2p/gateways`
/// document (`p2p_cluster_gateway.md` §8); returns the board fingerprint it published.
async fn publish_gateways(
    publisher: &control_radio::mqtt::RumqttcPublisher,
    board: &control_radio::p2p::gw_board::GatewayBoard,
) -> (u64, Vec<u64>) {
    use control_radio::northbound::RetainedPublisher;
    let seen = board.fingerprint();
    let now = control_radio::lorawan::gateway::now_unix();
    let now_u = u64::try_from(now).unwrap_or(0);
    let server = control_radio::northbound::hostname();
    let mut messages = Vec::new();
    for (gw, row) in board.remotes() {
        let alive = row.online && p2p_task::is_alive(&row.liveness, now_u);
        if let Ok(p) = serde_json::to_vec(&row.status.document(now, alive)) {
            messages.push((control_radio::p2p::northbridge::topic(&gw), p));
        }
    }
    if let Ok(p) = serde_json::to_vec(&board.document(&server, now)) {
        messages.push((control_radio::p2p::gw_board::topic(&server), p));
    }
    if let Err(e) = publisher.publish_retained(&messages).await {
        eprintln!("proximos-control: cannot publish the P2P gateways documents: {e}");
    }
    seen
}

/// Write `/run/proximos/radio/<name>` beside the heartbeat, by temp-and-rename, so a reader
/// never sees half a document. Best effort: the MQTT copy is the one the portal reads.
fn write_run_file(name: &str, payload: &[u8]) {
    let dir = control_radio::tasks::heartbeat::alive_path()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("/run/proximos/radio"));
    let tmp = dir.join(format!(".{name}.tmp"));
    let dest = dir.join(name);
    if let Err(e) = std::fs::write(&tmp, payload).and_then(|()| std::fs::rename(&tmp, &dest)) {
        eprintln!("proximos-control: cannot write {}: {e}", dest.display());
    }
}

/// Touch the liveness file, logging rather than failing — see the note in the tick.
fn touch_heartbeat() {
    // Last in the tick, so the file only advances once the work it vouches for is done.
    if let Err(e) = control_radio::tasks::heartbeat::touch() {
        // Not fatal, and deliberately so: a Hub whose `/run` is momentarily unwritable should
        // keep decoding uplinks. The consequence is visible on its own — `node-get` starts
        // answering `DaemonDown` — which is the honest report of "this daemon cannot prove it
        // is working".
        eprintln!("proximos-control: cannot touch the liveness file: {e}");
    }
}

/// Whether the heartbeat may vouch for this process's radio tasks: the health tick has done its
/// work (the caller runs this after it), and a hosted P2P central, if there is one, has heard
/// from the northbridge recently enough.
fn vouches_for_radio(p2p: Option<&control_radio::p2p::runtime::Liveness>, now_unix: u64) -> bool {
    p2p.is_none_or(|liveness| p2p_task::is_alive(liveness, now_unix))
}

/// How long a failed task waits before its first retry, and the ceiling it backs off to.
///
/// Named here rather than buried in `supervise` because the numbers are a boot-time contract:
/// on first boot mosquitto and ChirpStack may both still be starting, and this daemon must not
/// spin against them — nor take minutes to notice they arrived.
pub const BACKOFF_MIN: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;
    use control_radio::p2p::runtime::Liveness;

    #[test]
    fn the_heartbeat_waits_for_the_p2p_task() {
        let now = 1_800_000_000;
        assert!(
            vouches_for_radio(None, now),
            "no P2P arm, nothing to wait for"
        );

        let never = Liveness::default();
        assert!(
            !vouches_for_radio(Some(&never), now),
            "a central that has not heard from the northbridge withholds the heartbeat"
        );

        let live = Liveness::default();
        live.mark(now);
        assert!(vouches_for_radio(Some(&live), now));

        let gone = Liveness::default();
        gone.mark(now - 3600);
        assert!(!vouches_for_radio(Some(&gone), now));
    }
}
