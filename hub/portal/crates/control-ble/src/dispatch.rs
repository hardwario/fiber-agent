//! Dispatch — `plan/control/ble/plan.md` §1, §5.
//!
//! **The mode gate runs before anything else**: before argument validation, before any file is
//! opened, before the adapter is touched. So an `off` Hub answers `OffMode` rather than complaining
//! about a flag, and a disabled capability is decided before a command can occupy the adapter.
//!
//! The output contract (§5, resolving open question 9) is the one both dispatch groups share:
//! success is the result document on stdout, failure is the §6 error envelope, and the plain-text
//! shape *is* the pretty JSON — there is no second hand-formatted table to keep in sync.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::json;

use crate::central::{enroll, ledger::ProvisionLedger};
use crate::cli::{BeaconGetType, BeaconSendKind, Command, ControlBle};
use crate::config::{BleConfig, EyeSection};
use crate::error::{Component, Error, Kind, Result, Role};
use crate::host::{Host, Origin};
use crate::mac::MacAddr;
use crate::mode::resolve_scope;
use crate::peripheral::paging::{build_page, BeaconList};
use crate::status::{
    AdapterReport, CentralReport, CommandResult, En12830Report, PeripheralReport, SessionReport,
    StatusReport,
};
use crate::store::{Slot, Store};
use crate::tasks::LiveHandles;

/// The rendered device config. Not configurable in the shipped image: the unit file, the tmpfiles
/// fragment and the Landlock table all name it literally.
pub const DEFAULT_CONFIG_PATH: &str = "/run/proximos/control/config.yaml";
/// The fleet file the harness reads and writes — the path `security/landlock.md` §5.1 already lists.
pub const DEFAULT_FLEET_CONFIG_PATH: &str = "/data/proximos/config/fiber.config.yaml";

pub const ENV_CONFIG: &str = "PROXIMOS_CONFIG";
pub const ENV_BLE_DATA: &str = "PROXIMOS_BLE_DATA";
pub const ENV_FLEET_CONFIG: &str = "PROXIMOS_FLEET_CONFIG";

pub fn config_path() -> PathBuf {
    std::env::var_os(ENV_CONFIG)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
}

pub fn store_root() -> PathBuf {
    std::env::var_os(ENV_BLE_DATA)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(crate::store::DEFAULT_ROOT))
}

pub fn fleet_config_path() -> PathBuf {
    std::env::var_os(ENV_FLEET_CONFIG)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_FLEET_CONFIG_PATH))
}

/// Read and validate the `ble:` section.
pub fn load_config() -> Result<BleConfig> {
    let path = config_path();
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        Error::new(
            Kind::NotBootstrapped,
            None,
            Component::DeviceConfig,
            format!(
                "cannot read the rendered device config {}: {e}",
                path.display()
            ),
        )
    })?;
    BleConfig::from_device_config_yaml(&raw)
}

/// What a command produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Status(Box<StatusReport>),
    Json(serde_json::Value),
    Text(String),
}

impl Outcome {
    /// The document, whichever shape it came in.
    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Outcome::Status(s) => serde_json::to_value(s).unwrap_or(serde_json::Value::Null),
            Outcome::Json(v) => v.clone(),
            Outcome::Text(t) => serde_json::Value::String(t.clone()),
        }
    }
}

fn daemon_down(command: &str) -> Error {
    Error::new(
        Kind::DaemonDown,
        None,
        Component::ControlSocket,
        format!(
            "`{command}` needs the running tasks (the scan loop, the dial queue or the FB00 server) \
             and no process is hosting them here"
        ),
    )
}

fn not_found(mac: &MacAddr) -> Error {
    Error::new(
        Kind::NotFound,
        Role::Central,
        Component::BeaconRegistry,
        format!("{mac} is not enrolled"),
    )
}

/// `run`, with the shipped store root.
pub async fn run(
    cli: &ControlBle,
    config: &BleConfig,
    host: &Host,
    live: Option<&LiveHandles>,
) -> Result<Outcome> {
    run_with_store(cli, config, host, live, Store::new(store_root())).await
}

pub async fn run_with_store(
    cli: &ControlBle,
    config: &BleConfig,
    host: &Host,
    live: Option<&LiveHandles>,
    store: Store,
) -> Result<Outcome> {
    // THE ORDER MATTERS. The mode gate is first: an `off` Hub must not answer a malformed `--mac`
    // with `InvalidArgument`, because the honest answer is that no BLE role exists here at all.
    let scope = resolve_scope(
        config.mode,
        cli.command.requested_role(),
        cli.command.role_arity(),
    )?;
    cli.command.validate()?;

    // For a Fixed command the scope is its own role; for a spanning one (`create`, `status`) the
    // envelope names the first role in scope and the body covers all of them.
    let role_str = match scope.as_slice() {
        [only] => only.as_str(),
        _ => "both",
    };
    let name = cli.command.name();

    match &cli.command {
        Command::Create(a) => create(config, host, live, &store, a.dry_run, &scope, role_str).await,

        Command::Status(_) => Ok(Outcome::Status(Box::new(
            status(config, host, live, &store, &scope).await,
        ))),

        Command::BeaconAdd(a) => {
            let out = enroll::add(host, Origin::Cli, &a.mac, a.name.as_deref())?;
            // The enrollment is on disk either way; the follow-up dial needs the running
            // component's queue, which a one-shot CLI without a daemon does not have — so it is
            // reported, not an error.
            let follow_up_queued = live
                .and_then(|l| l.central.as_ref())
                .is_some_and(|central| {
                    central
                        .queue(crate::tasks::DialJob {
                            mac: out.mac,
                            kind: out.follow_up.dial_kind(),
                        })
                        .is_ok()
                });
            Ok(Outcome::Json(
                CommandResult::new(name, role_str)
                    .with("mac", out.mac.to_string())
                    .with_json("name", &out.name)
                    .with("monitoring", out.monitoring.as_str())
                    .with("already_enrolled", out.already_enrolled)
                    .with("follow_up", out.follow_up.as_str())
                    .with("follow_up_queued", follow_up_queued)
                    .to_value(),
            ))
        }

        Command::BeaconRemove(a) => {
            let out = enroll::remove(host, Origin::Cli, &a.mac)?;
            // A one-shot CLI has no central role to forget the beacon through, so the ledger record
            // goes here; with a daemon hosting the role it is already gone and this is a no-op.
            let mut ledger = ProvisionLedger::load(&store)?;
            if ledger.reset(&out.mac) {
                ledger.save(&store)?;
            }
            if !out.removed {
                // Unlike FB10, the CLI has an exit code to say this with — and a shell script that
                // removed nothing needs to know.
                return Err(not_found(&out.mac));
            }
            Ok(Outcome::Json(
                CommandResult::new(name, role_str)
                    .with("mac", out.mac.to_string())
                    .with("removed", true)
                    .to_value(),
            ))
        }

        Command::BeaconMonitoring(a) => {
            let out = enroll::set_monitoring(host, Origin::Cli, a.enabled)?;
            Ok(Outcome::Json(
                CommandResult::new(name, role_str)
                    .with("enabled", out.enabled)
                    .with("changed", out.changed)
                    .with("monitoring", out.monitoring.as_str())
                    .to_value(),
            ))
        }

        Command::BeaconList(a) => {
            let section = host.config.load()?;
            let list = BeaconList::from_tags(&section.tags);
            match a.page {
                // Byte-for-byte what FB0F answers at that MTU — which is what makes the paging
                // contract checkable without a phone.
                Some(page) => {
                    let read = build_page(&list, page, a.mtu, config.peripheral.max_page_bytes);
                    Ok(Outcome::Json(
                        serde_json::to_value(&read).unwrap_or(serde_json::Value::Null),
                    ))
                }
                None => Ok(Outcome::Json(json!({
                    "total": list.len(),
                    "rev": list.rev,
                    "items": list.items,
                }))),
            }
        }

        Command::BeaconInfo(a) => {
            let mac = MacAddr::parse(&a.mac)?;
            let section = host.config.load()?;
            let tag = section.tag(&mac.to_string());
            let ledger = ProvisionLedger::load(&store)?;
            let record = ledger.get(&mac);
            if tag.is_none() && record.is_none() {
                return Err(not_found(&mac));
            }
            Ok(Outcome::Json(json!({
                "mac": mac.to_string(),
                "enrollment": tag,
                "provisioning": record,
            })))
        }

        Command::BeaconGet(a) => {
            let mac = MacAddr::parse(&a.mac)?;
            let central = live
                .and_then(|l| l.central.as_ref())
                .ok_or_else(|| daemon_down(name))?;
            let state = crate::central::state::read_state(&central.state);
            let beacon = state.beacons.get(&mac).ok_or_else(|| not_found(&mac))?;
            let body = match a.what {
                BeaconGetType::Reading => {
                    serde_json::to_value(beacon.reading).unwrap_or(serde_json::Value::Null)
                }
                BeaconGetType::Axes => json!({
                    "discovered": beacon.discovered,
                    "provisioning": beacon.provisioning.as_str(),
                    "is_en12830": beacon.is_en12830,
                }),
                BeaconGetType::History => json!({
                    "last_archived_ts": beacon.last_archived_ts,
                    "last_download_ts": beacon.last_download_ts,
                    "archive_file": store.archive_path(&mac).display().to_string(),
                }),
            };
            Ok(Outcome::Json(json!({
                "mac": mac.to_string(),
                "type": a.what.as_str(),
                "value": body,
            })))
        }

        Command::BeaconScanList => {
            let central = live
                .and_then(|l| l.central.as_ref())
                .ok_or_else(|| daemon_down(name))?;
            let enrolled = enrolled_set(&host.config.load()?);
            let state = crate::central::state::read_state(&central.state);
            let known = state.known_beacons.clone();
            let members: Vec<serde_json::Value> = state
                .beacons
                .iter()
                .filter(|(m, _)| !enrolled.contains(m))
                .map(|(m, b)| {
                    json!({
                        "mac": m.to_string(),
                        "rssi": b.rssi,
                        "last_seen_ts": b.last_seen_ts,
                        // The operator-relevant distinction: "mine, unenrolled" vs "someone
                        // else's fleet" (central.md §5's foreign-beacon etiquette).
                        "known": known.contains(m),
                    })
                })
                .collect();
            Ok(Outcome::Json(json!({
                "count": members.len(),
                "discovered_max": config.central.discovered_max,
                "discovered_capped": state.discovered_capped,
                "beacons": members,
            })))
        }

        Command::BeaconScanStart => {
            let central = live
                .and_then(|l| l.central.as_ref())
                .ok_or_else(|| daemon_down(name))?;
            if central.is_scanning() {
                return Err(Error::new(
                    Kind::AlreadyExists,
                    Role::Central,
                    Component::BluetoothAdapter,
                    "the scan loop is already running",
                ));
            }
            let started = central.start_scan();
            Ok(Outcome::Json(
                CommandResult::new(name, role_str)
                    .with("started", started)
                    .to_value(),
            ))
        }

        Command::BeaconScanStop => {
            let central = live
                .and_then(|l| l.central.as_ref())
                .ok_or_else(|| daemon_down(name))?;
            central.stop_scan();
            Ok(Outcome::Json(
                CommandResult::new(name, role_str)
                    .with("scanning", central.is_scanning())
                    .to_value(),
            ))
        }

        Command::BeaconProvision(a) => {
            let mac = MacAddr::parse(&a.mac)?;
            // The operator action central.md §5 means by "left alone until an operator acts": the
            // budget reopens, and it is recorded before the dial so a crash mid-dial is not a
            // silently-still-exhausted beacon. The same call the signed `beacon-provision` makes.
            let reset = ProvisionLedger::load(&store)?
                .get(&mac)
                .is_some_and(|r| r.attempts > 0 || r.gave_up_at.is_some());
            // Only the Beacon's home Hub talks to it (decision #42).
            crate::central::cluster::check_home(host, &mac)?;
            let central = live
                .and_then(|l| l.central.as_ref())
                .ok_or_else(|| daemon_down(name))?;
            central.request_provision(&mac)?;
            Ok(Outcome::Json(
                CommandResult::new(name, role_str)
                    .with("mac", mac.to_string())
                    .with("budget_reset", reset)
                    .with("queued", true)
                    .to_value(),
            ))
        }

        Command::BeaconSend(a) => {
            let BeaconSendKind::Config(c) = &a.what;
            let mac = MacAddr::parse(&c.mac)?;
            let sensors = c
                .sensors
                .as_deref()
                .map(crate::cli::parse_sensors)
                .transpose()?;
            let profile = crate::central::provision::Profile::partial(
                c.advertising_interval_s,
                c.tx_power_dbm,
                sensors.as_deref(),
            );
            crate::central::cluster::check_home(host, &mac)?;
            let central = live
                .and_then(|l| l.central.as_ref())
                .ok_or_else(|| daemon_down(name))?;
            central
                .queue(crate::tasks::DialJob {
                    mac,
                    kind: crate::tasks::DialKind::ConfigOverride(profile),
                })
                .map_err(|_| {
                    Error::new(
                        Kind::AlreadyExists,
                        Role::Central,
                        Component::BluetoothAdapter,
                        "the dial queue is full",
                    )
                })?;
            // `written`/`skipped` are the dial's own result and reach the operator through the
            // snapshot; the command reports what it asked for.
            let mut requested: Vec<&'static str> = Vec::new();
            if c.advertising_interval_s.is_some() {
                requested.push("advertising_interval_s");
            }
            if c.tx_power_dbm.is_some() {
                requested.push("tx_power_dbm");
            }
            if c.sensors.is_some() {
                requested.push("active_sensors");
            }
            Ok(Outcome::Json(
                CommandResult::new(name, role_str)
                    .with("mac", mac.to_string())
                    .with_json("requested", &requested)
                    .with("queued", true)
                    .to_value(),
            ))
        }

        Command::BeaconDetect(a) => {
            let mac = MacAddr::parse(&a.mac)?;
            archive_command(host, live, &store, name, role_str, mac, None, None).await
        }

        Command::BeaconHistory(a) => {
            let mac = MacAddr::parse(&a.mac)?;
            archive_command(host, live, &store, name, role_str, mac, None, Some(a)).await
        }

        Command::BeaconRecording(a) => {
            let mac = MacAddr::parse(&a.mac)?;
            archive_command(
                host,
                live,
                &store,
                name,
                role_str,
                mac,
                Some(a.interval_min),
                None,
            )
            .await
        }

        Command::KnownBeacons(a) => {
            // **Validate and report only, never install.** The allowlist is daemon state and is
            // deliberately never written to config: a beacon enrolled on another Hub must become
            // audible here without this Hub claiming ownership of it.
            let mut accepted = Vec::new();
            for entry in a.entries() {
                accepted.push(MacAddr::parse(entry)?.to_string());
            }
            Ok(Outcome::Json(
                CommandResult::new(name, role_str)
                    .with_json("accepted", &accepted)
                    .with("count", accepted.len())
                    .with("installed", false)
                    .with(
                        "note",
                        "the fleet allowlist is daemon state; this reports what the daemon would \
                         accept and cannot install it",
                    )
                    .to_value(),
            ))
        }

        Command::PhoneSessionStart(a) => {
            let peripheral = live
                .and_then(|l| l.peripheral.as_ref())
                .ok_or_else(|| daemon_down(name))?;
            let idle = a
                .idle_timeout_s
                .map(u64::from)
                .unwrap_or(config.peripheral.session_idle_timeout_s);
            let hostname = host.device.hostname();
            // The **real** adapter address, probed now. A QR code carrying all-zeros would send a
            // phone looking for a Hub that does not exist, and the payload is the one thing an
            // operator cannot sanity-check by eye.
            let mac = probe_adapter(config)
                .await
                .address
                .and_then(|a| MacAddr::parse(&a).ok())
                .ok_or_else(|| {
                    Error::new(
                        Kind::AdapterDown,
                        Role::Peripheral,
                        Component::BluetoothAdapter,
                        "cannot mint a provisioning session without the controller address: the QR \
                         code the phone scans is addressed to it",
                    )
                })?;
            let preview = peripheral
                .session
                .start(host.clock.as_ref(), idle, &mac, &hostname);
            Ok(Outcome::Json(
                CommandResult::new(name, role_str)
                    // Masked. The real token reaches a human only via the LCD.
                    .with_json("qr_payload_preview", &preview)
                    .with("expires_at", preview.exp)
                    .to_value(),
            ))
        }

        Command::PhoneSessionStop => {
            let peripheral = live
                .and_then(|l| l.peripheral.as_ref())
                .ok_or_else(|| daemon_down(name))?;
            let ended = peripheral.session.stop();
            Ok(Outcome::Json(
                CommandResult::new(name, role_str)
                    .with("ended", ended)
                    .to_value(),
            ))
        }

        Command::PhoneSessionStatus => {
            let peripheral = live
                .and_then(|l| l.peripheral.as_ref())
                .ok_or_else(|| daemon_down(name))?;
            let s = peripheral.session.status(host.clock.as_ref());
            let peer = peripheral.connected_peer.lock().await.map(fmt_peer);
            Ok(Outcome::Json(
                serde_json::to_value(SessionReport {
                    active: s.active,
                    expires_at: s.expires_at,
                    authenticated: peer.is_some(),
                    peer,
                })
                .unwrap_or(serde_json::Value::Null),
            ))
        }

        Command::PhoneAdvertiseStart | Command::PhoneAdvertiseStop => {
            let peripheral = live
                .and_then(|l| l.peripheral.as_ref())
                .ok_or_else(|| daemon_down(name))?;
            let changed = if matches!(cli.command, Command::PhoneAdvertiseStart) {
                peripheral.start_advertising()
            } else {
                peripheral.stop_advertising()
            };
            Ok(Outcome::Json(
                CommandResult::new(name, role_str)
                    .with("changed", changed)
                    .with("advertising", peripheral.is_advertising())
                    .to_value(),
            ))
        }
    }
}

/// The three archive commands, which share a gate order that matters.
#[allow(clippy::too_many_arguments)]
async fn archive_command(
    host: &Host,
    live: Option<&LiveHandles>,
    _store: &Store,
    name: &'static str,
    role_str: &'static str,
    mac: MacAddr,
    interval_min: Option<u16>,
    history: Option<&crate::cli::BeaconHistoryArgs>,
) -> Result<Outcome> {
    // **The capability flag is checked before anything else** — before the daemon check, before the
    // adapter. A command that persists a setting, prints success and blames a missing subsystem is
    // the specific failure central.md §6 exists to prevent, and answering `DaemonDown` for a
    // disabled capability would be the same mistake in a different coat.
    let section = host.config.load()?;
    if !section.en12830.enabled {
        return Err(Error::new(
            Kind::CapabilityDisabled,
            Role::Central,
            Component::BeaconGatt,
            format!(
                "`{name}` needs the EN12830 archive capability, which is disabled \
                 (eye.en12830.enabled: false)"
            ),
        ));
    }
    // A refused interval is a usage error, never snapped to a neighbour.
    if let Some(min) = interval_min {
        if min != 0 {
            crate::central::en12830::protocol::interval_s(min)?;
        }
    }

    let request = history
        .map(|a| {
            crate::central::history::HistoryRequest::new(
                a.from,
                a.to,
                a.job.clone(),
                host.clock.now_unix(),
            )
        })
        .transpose()?;

    // Only the Beacon's home Hub talks to it (decision #42). A download asked of another Hub
    // ends at once as `failed: not_home`, so the portal's job closes and it asks the home.
    if !section.allows_gatt(&mac) {
        if let Some(request) = &request {
            host.publisher.publish_history(&request.message(
                &mac,
                crate::central::history::HistoryBody::Failed {
                    reason: crate::central::history::FailReason::NotHome,
                },
            ));
        }
        return Err(crate::central::cluster::not_home_refusal(&mac));
    }

    let central = live
        .and_then(|l| l.central.as_ref())
        .ok_or_else(|| daemon_down(name))?;
    // A download is announced on `eye/history` and, on a full queue, retried rather than refused
    // (beacon_history.md §2.4) — so its answer carries the job id the messages will carry.
    if let Some(request) = request {
        let job = request.job.clone();
        let queued = central.request_history(
            &mac,
            request,
            host.publisher.as_ref(),
            host.clock.monotonic_secs(),
        );
        return Ok(Outcome::Json(
            CommandResult::new(name, role_str)
                .with("mac", mac.to_string())
                .with("job", job)
                .with("queued", queued)
                .to_value(),
        ));
    }
    let kind = match (name, interval_min) {
        ("beacon-recording", Some(min)) => crate::tasks::DialKind::Recording(min),
        _ => crate::tasks::DialKind::Detect,
    };
    central
        .queue(crate::tasks::DialJob { mac, kind })
        .map_err(|_| {
            Error::new(
                Kind::AlreadyExists,
                Role::Central,
                Component::BluetoothAdapter,
                "the dial queue is full",
            )
        })?;
    Ok(Outcome::Json(
        CommandResult::new(name, role_str)
            .with("mac", mac.to_string())
            .with("queued", true)
            .to_value(),
    ))
}

fn fmt_peer(p: [u8; 6]) -> String {
    MacAddr::from(p).to_string()
}

fn enrolled_set(section: &EyeSection) -> BTreeSet<MacAddr> {
    section
        .tags
        .iter()
        .filter(|t| t.enabled)
        .filter_map(|t| MacAddr::parse(&t.mac).ok())
        .collect()
}

/// Probe the adapter without failing when it is absent.
async fn probe_adapter(config: &BleConfig) -> AdapterReport {
    let Ok(session) = bluer::Session::new().await else {
        return AdapterReport {
            present: false,
            address: None,
            powered: false,
        };
    };
    let adapter = match &config.adapter {
        Some(name) => session.adapter(name),
        None => match session.default_adapter().await {
            Ok(a) => Ok(a),
            Err(e) => Err(e),
        },
    };
    match adapter {
        // `session.adapter(name)` is **lazy** in bluer: it hands back a handle without checking that
        // the adapter exists, so `present` has to be established by actually reading something off
        // it. Without this a typo'd `ble.adapter` reports a controller that is not there — and
        // degraded-green is only useful if it is honest about which state it is in.
        Ok(a) => match a.address().await {
            Ok(addr) => AdapterReport {
                present: true,
                address: Some(addr.to_string()),
                powered: a.is_powered().await.unwrap_or(false),
            },
            Err(_) => AdapterReport {
                present: false,
                address: None,
                powered: false,
            },
        },
        Err(_) => AdapterReport {
            present: false,
            address: None,
            powered: false,
        },
    }
}

async fn status(
    config: &BleConfig,
    host: &Host,
    live: Option<&LiveHandles>,
    store: &Store,
    scope: &[Role],
) -> StatusReport {
    let adapter = probe_adapter(config).await;
    // A failure to read the fleet section must not fail `status` — it is one of the things being
    // reported on.
    let section = host.config.load().unwrap_or_default();

    let central = scope.contains(&Role::Central).then(|| {
        let (scanning, beacons, discovered, capped, unparseable, failures) =
            match live.and_then(|l| l.central.as_ref()) {
                Some(c) => {
                    let scanning = c.is_scanning();
                    let failures = c.capability.worst_streak();
                    // A blocking read here would deadlock a single-threaded caller; `try_read` failing
                    // means the loop is mid-tick, which is not worth blocking a status call for.
                    match c.state.try_read() {
                        Ok(s) => (
                            scanning,
                            s.beacons.len() as u32,
                            s.beacons.len() as u32
                                - s.beacons
                                    .keys()
                                    .filter(|m| enrolled_set(&section).contains(m))
                                    .count() as u32,
                            s.discovered_capped,
                            s.unparseable_frames,
                            failures,
                        ),
                        Err(_) => (scanning, 0, 0, false, 0, failures),
                    }
                }
                None => (false, 0, 0, false, 0, 0),
            };
        CentralReport {
            provisioned: store.is_provisioned(Role::Central),
            scanning,
            enabled: section.enabled,
            beacons,
            discovered,
            discovered_capped: capped,
            unparseable_frames: unparseable,
            en12830: En12830Report {
                enabled: section.en12830.enabled,
                consecutive_failures: failures,
            },
        }
    });

    let peripheral = scope.contains(&Role::Peripheral).then(|| {
        let (advertising, session) = match live.and_then(|l| l.peripheral.as_ref()) {
            Some(p) => {
                let s = p.session.status(host.clock.as_ref());
                (
                    p.is_advertising(),
                    SessionReport {
                        active: s.active,
                        expires_at: s.expires_at,
                        authenticated: false,
                        peer: None,
                    },
                )
            }
            None => (
                false,
                SessionReport {
                    active: false,
                    expires_at: None,
                    authenticated: false,
                    peer: None,
                },
            ),
        };
        PeripheralReport {
            provisioned: store.is_provisioned(Role::Peripheral),
            advertising,
            session,
        }
    });

    StatusReport {
        ble_mode: config.mode.as_str(),
        adapter,
        central,
        peripheral,
    }
}

async fn create(
    config: &BleConfig,
    host: &Host,
    live: Option<&LiveHandles>,
    store: &Store,
    dry_run: bool,
    scope: &[Role],
    role_str: &'static str,
) -> Result<Outcome> {
    let adapter = probe_adapter(config).await;
    let mut warnings: Vec<String> = Vec::new();
    let mut provisioned = serde_json::Map::new();
    let roles: Vec<&'static str> = scope.iter().map(|r| r.as_str()).collect();

    if !dry_run {
        store.ensure_dirs()?;
    }

    for role in scope {
        let ok = match role {
            Role::Central => {
                if !dry_run {
                    // Create the ledger if it is absent, so the first give-up has somewhere to go.
                    if store.read(Slot::Beacons)?.is_none() {
                        ProvisionLedger::default().save(store)?;
                    }
                }
                let scanning = live
                    .and_then(|l| l.central.as_ref())
                    .is_some_and(|c| c.is_scanning());
                if !adapter.present {
                    warnings.push(
                        "no Bluetooth controller: central role idle (degraded-green)".to_string(),
                    );
                    false
                } else if !scanning {
                    warnings.push(
                        "the scan loop is not running in this process: the central marker is \
                         withheld so the next boot re-probes"
                            .to_string(),
                    );
                    false
                } else {
                    true
                }
            }
            Role::Peripheral => {
                let advertising = live
                    .and_then(|l| l.peripheral.as_ref())
                    .is_some_and(|p| p.is_advertising());
                if !adapter.present {
                    warnings.push(
                        "no Bluetooth controller: peripheral role idle (degraded-green)"
                            .to_string(),
                    );
                    false
                } else if !advertising {
                    warnings.push(
                        "FB00 registration happens inside the hosting process; the peripheral \
                         marker is withheld"
                            .to_string(),
                    );
                    false
                } else {
                    true
                }
            }
        };
        // The marker records provisioned-complete, not degraded-green: withholding it is what makes
        // the next boot re-probe instead of assuming.
        if ok && !dry_run {
            store.mark_provisioned(*role)?;
        }
        provisioned.insert(role.as_str().to_string(), serde_json::Value::Bool(ok));
    }

    let _ = host;
    Ok(Outcome::Json(
        CommandResult::new("create", role_str)
            .with_json("roles", &roles)
            .with("dry_run", dry_run)
            .with("provisioned", serde_json::Value::Object(provisioned))
            .with_json("warnings", &warnings)
            .with_json("adapter", &adapter)
            .to_value(),
    ))
}
