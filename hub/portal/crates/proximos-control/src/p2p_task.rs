//! The P2P central, hosted here — PI1 of `plan/portal/p2p_integration.md` (§4.1).
//!
//! Decision #12 puts the P2P loop duties in this process, beside the LoRaWAN ones. The central
//! itself is `control_radio::p2p::runtime`; this module only decides whether it runs, opens the
//! northbridge UART and hands the runtime a stop signal. LoRa P2P runs over the northbridge
//! alone (`/dev/ttyAMA3`); the RAK5146 concentrator is LoRaWAN's and is never touched here.
//!
//! Before the UART opens for the central, [`nb_update`] compares the Northbridge firmware with
//! the image's bundled one and flashes it when they differ (`northbridge_update.md` §3.4).
//!
//! On a cluster Client the central does not run: the same task runs the gateway forwarder
//! ([`run_gateway`], `plan/control/radio/p2p_cluster_gateway.md` §3.2), lending this Hub's
//! Northbridge to the Server's central over the gateway plane.
//!
//! One run is one open UART. A `modem link error` — the northbridge rebooting, the UART held by
//! a bench host — ends the run and the supervisor retries it with the same backoff as the other
//! tasks. The broker client and the [`Liveness`] handle outlive the runs: both are created once
//! in `main`, so a restart neither opens a second broker session under the same client id nor
//! leaves the health loop reading a handle nobody updates. The gateway plane's session is the
//! exception: it lives for one run (a remote gateway's modem is only valid inside it) and ends
//! with it, before the next run opens its own.

use std::sync::Arc;

use control_radio::config::RadioConfig;
use control_radio::p2p::gateway_fwd::{self, ClientBroker, ForwarderConfig};
use control_radio::p2p::gw_board::GatewayBoard;
use control_radio::p2p::modem_io::SerialModem;
use control_radio::p2p::nb_update;
use control_radio::p2p::northbridge::NorthbridgeStatus;
use control_radio::p2p::outbox::Outbox;
use control_radio::p2p::publisher::RumqttcPublisher;
use control_radio::p2p::remote::{self, PlaneHub};
use control_radio::p2p::runtime::{run_central_multi_until, Central, Liveness};
use control_radio::store::Store;
use control_radio::Transport;

use crate::supervise::Shutdown;

/// The broker client id of the hosted central — distinct from the bench host's
/// `control-radio-p2p`, so the two can never kick each other off the broker.
pub const CLIENT_ID: &str = "proximos-control-p2p";

/// Whether this Hub runs a P2P task at all: whenever `radio.mode` allows the P2P transport.
/// Which one — the central or the gateway forwarder — is [`gateway_role`]'s question.
pub fn enabled(config: &RadioConfig) -> bool {
    config.mode.allows(Transport::P2p)
}

/// The Server's broker when this Hub is a cluster Client, `None` on a Server.
///
/// The rendered config is the only witness (decision #11: the renderer alone reads the role
/// marker): a Client's carries the agent broker override (cluster.md §4.4). A Client runs the
/// gateway forwarder instead of the central (`p2p_cluster_gateway.md` §3.2).
pub fn gateway_role() -> Option<ClientBroker> {
    let raw = std::fs::read_to_string(control_radio::dispatch::config_path()).ok()?;
    ClientBroker::from_device_config_yaml(&raw)
}

/// The image this Hub runs, for the gateway's state document: `VERSION_ID` (or `VERSION`) of
/// `/etc/os-release`.
pub(crate) fn image_version() -> Option<String> {
    let raw = std::fs::read_to_string("/etc/os-release").ok()?;
    let field = |key: &str| {
        raw.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_string())
            .filter(|v| !v.is_empty())
    };
    field("VERSION_ID").or_else(|| field("VERSION"))
}

/// Whether the P2P half of this process is alive at `now_unix`, for the heartbeat.
///
/// The central marks [`Liveness`] on every modem event and on every idle `GET_INFO` probe, so a
/// healthy central is never more than a minute stale even on a network with no traffic. Allowed
/// [`control_radio::tasks::heartbeat::STALE_MULTIPLIER`] health intervals, the same patience
/// `node-get` has with the heartbeat file itself.
pub fn is_alive(liveness: &Liveness, now_unix: u64) -> bool {
    let interval = control_radio::tasks::health::interval().as_secs();
    let max_age =
        interval.saturating_mul(u64::from(control_radio::tasks::heartbeat::STALE_MULTIPLIER));
    liveness.is_fresh(now_unix, max_age)
}

/// The central's northbound outbox under the state root, with what the last process left.
/// Opened once per process, beside the broker client.
pub fn open_outbox() -> Arc<Outbox> {
    Arc::new(Outbox::open(&Store::new(
        control_radio::dispatch::store_root(),
    )))
}

/// One run of the central: build it from the config, open the UART, and run until shutdown
/// or a dead link. Every way the run can end without reaching the modem is recorded in
/// `status` (`northbridge_diagnostics.md` §3), so the portal says why rather than going quiet.
pub async fn run(
    config: Arc<RadioConfig>,
    publisher: Arc<RumqttcPublisher>,
    liveness: Liveness,
    outbox: Arc<Outbox>,
    status: NorthbridgeStatus,
    board: GatewayBoard,
    shutdown: Shutdown,
) -> control_radio::Result<()> {
    let uart = control_radio::p2p::uart::device_path();
    status.starting(&uart);
    let result = run_once(
        config,
        publisher,
        liveness,
        outbox,
        status.clone(),
        board,
        shutdown,
    )
    .await;
    if let Err(e) = &result {
        status.unreachable(control_radio::lorawan::gateway::now_unix(), e.message());
    }
    result
}

/// Build the central, bring the Northbridge firmware in line (`northbridge_update.md` §3.4,
/// U4), open the modem and run until shutdown, a dead link or an update request.
///
/// The update runs between the build and the open: a Hub whose P2P arm is not provisioned
/// fails the build without touching the UART, and the flasher has the port to itself because
/// the modem is not open yet. Its outcome never stops the central — [`nb_update::ensure`]
/// reports rather than fails. A `northbridge-update` request from the CLI stops the central
/// the same way shutdown does; the request is answered and the central rebuilt within this
/// run, so the supervisor sees no exit and applies no backoff.
async fn run_once(
    config: Arc<RadioConfig>,
    publisher: Arc<RumqttcPublisher>,
    liveness: Liveness,
    outbox: Arc<Outbox>,
    status: NorthbridgeStatus,
    board: GatewayBoard,
    shutdown: Shutdown,
) -> control_radio::Result<()> {
    let uart = control_radio::p2p::uart::device_path();
    let store = Store::new(control_radio::dispatch::store_root());
    // The newest request this process answered, so one whose result could not be saved is not
    // run again on every rebuild.
    let mut handled = 0u64;
    loop {
        let central = Central::from_config(store.clone(), &config, publisher.clone())?
            .with_liveness(liveness.clone())
            .with_outbox(outbox.clone())
            .with_status(status.clone())
            .with_board(board.clone());
        let request = nb_update::pending_request(&store, handled);
        nb_update::ensure(&uart, &store, &status, request.as_ref()).await;
        if let Some(r) = &request {
            handled = handled.max(r.generation);
        }
        // Opened only here, after the central is built: a Hub whose P2P arm is not provisioned
        // yet fails above without holding the northbridge's UART.
        let modem = SerialModem::open_default()?;
        // The cluster Clients' Northbridges join over the gateway plane as they come online
        // (`p2p_cluster_gateway.md` §4). The plane session and its bridge end with this run.
        let (plane, plane_events) = PlaneHub::connect(control_radio::northbound::hostname());
        let (changes_tx, changes_rx) = tokio::sync::mpsc::channel(16);
        let bridge = tokio::spawn(remote::follow(
            plane,
            plane_events,
            changes_tx,
            board.clone(),
        ));
        let stop = {
            let shutdown = shutdown.clone();
            let store = store.clone();
            async move {
                tokio::select! {
                    _ = shutdown.wait() => {}
                    _ = nb_update::wait_for_request(&store, handled) => {}
                }
            }
        };
        let result = run_central_multi_until(central, Box::new(modem), changes_rx, stop).await;
        bridge.abort();
        result?;
        if shutdown.is_shutting_down() || nb_update::pending_request(&store, handled).is_none() {
            return Ok(());
        }
        status.starting(&uart);
    }
}

/// One run of the gateway forwarder on a cluster Client (`p2p_cluster_gateway.md` §3.2): the
/// Northbridge firmware check, the UART, then the bridge until shutdown, a dead UART or an
/// update request. The broker session lives inside the run and rides out Server outages on
/// its own reconnect; the Northbridge keeps ACKing its home peers meanwhile.
pub async fn run_gateway(
    broker: ClientBroker,
    status: NorthbridgeStatus,
    shutdown: Shutdown,
) -> control_radio::Result<()> {
    let uart = control_radio::p2p::uart::device_path();
    status.starting(&uart);
    let result = run_gateway_once(broker, status.clone(), shutdown).await;
    if let Err(e) = &result {
        status.unreachable(control_radio::lorawan::gateway::now_unix(), e.message());
    }
    result
}

async fn run_gateway_once(
    broker: ClientBroker,
    status: NorthbridgeStatus,
    shutdown: Shutdown,
) -> control_radio::Result<()> {
    let uart = control_radio::p2p::uart::device_path();
    let store = Store::new(control_radio::dispatch::store_root());
    let gw = control_radio::northbound::hostname();
    let mut handled = 0u64;
    loop {
        // The credentials first: a Client whose unit does not hand them over says so before
        // the UART is touched.
        let cfg = ForwarderConfig::from_credentials(gw.clone(), broker.clone(), image_version())?;
        let request = nb_update::pending_request(&store, handled);
        nb_update::ensure(&uart, &store, &status, request.as_ref()).await;
        if let Some(r) = &request {
            handled = handled.max(r.generation);
        }
        let nb = control_radio::p2p::uart::Northbridge::open_default()?;
        let stop = {
            let shutdown = shutdown.clone();
            let store = store.clone();
            async move {
                tokio::select! {
                    _ = shutdown.wait() => gateway_fwd::Stop::Shutdown,
                    _ = nb_update::wait_for_request(&store, handled) => gateway_fwd::Stop::Update,
                }
            }
        };
        gateway_fwd::run(cfg, nb, status.clone(), stop).await?;
        if shutdown.is_shutting_down() || nb_update::pending_request(&store, handled).is_none() {
            return Ok(());
        }
        status.starting(&uart);
    }
}

/// The Northbridge on a Hub whose `radio.mode` runs no P2P task (#158,
/// `northbridge_update.md` amendment 2026-10-28). The Northbridge is fitted on every Hub, so
/// its firmware is read and updated whatever the mode: one `GET_INFO` on start and after
/// every update, and a `northbridge-update` request answered with [`nb_update::ensure`]. The
/// UART is open only for those moments; nothing else drives it in this mode. There is no
/// automatic update here: the bundled image still goes on the first time the Hub runs P2P,
/// as before; a LoRaWAN Hub's Northbridge is flashed only when someone asks.
pub async fn run_idle(status: NorthbridgeStatus, shutdown: Shutdown) -> control_radio::Result<()> {
    use control_radio::p2p::modem_io::Modem;

    let uart = control_radio::p2p::uart::device_path();
    let store = Store::new(control_radio::dispatch::store_root());
    let bundle = nb_update::bundle_dir();
    let mut handled = 0u64;
    status.idle();
    loop {
        if let Some(r) = nb_update::pending_request(&store, handled) {
            handled = handled.max(r.generation);
            nb_update::ensure(&uart, &store, &status, Some(&r)).await;
        }
        let now = control_radio::lorawan::gateway::now_unix();
        // The port is dropped at the end of the block, before the wait.
        let probe = async {
            let mut modem = SerialModem::open_default()?;
            modem.get_info().await
        }
        .await;
        match probe {
            Ok(info) => {
                status.on_info(info, now);
                status.idle();
            }
            Err(e) => status.unreachable(now, e.message()),
        }
        status.set_update(nb_update::idle_summary(&store, &bundle));
        tokio::select! {
            _ = shutdown.wait() => return Ok(()),
            _ = nb_update::wait_for_request(&store, handled) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use control_radio::Mode;

    fn config(mode: Mode) -> RadioConfig {
        let yaml = include_str!("../../control-radio/tests/integration/radio.yaml");
        let mut cfg = RadioConfig::from_device_config_yaml(yaml).expect("the bench config parses");
        cfg.mode = mode;
        cfg
    }

    #[test]
    fn the_p2p_task_is_spawned_only_when_the_mode_allows_it() {
        assert!(!enabled(&config(Mode::Off)));
        assert!(!enabled(&config(Mode::Lorawan)));
        assert!(enabled(&config(Mode::P2p)));
        assert!(enabled(&config(Mode::Dual)));
    }

    #[test]
    fn a_modem_never_heard_from_is_not_alive() {
        assert!(!is_alive(&Liveness::default(), 1_800_000_000));
    }

    #[test]
    fn the_p2p_half_is_alive_for_three_health_intervals() {
        let interval = control_radio::tasks::health::interval().as_secs();
        let live = Liveness::default();
        live.mark(1_800_000_000);
        assert!(is_alive(&live, 1_800_000_000 + 3 * interval));
        assert!(!is_alive(&live, 1_800_000_000 + 3 * interval + 1));
    }
}
