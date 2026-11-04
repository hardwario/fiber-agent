//! The published command surface — `plan/control/radio/plan.md` §5.
//!
//! Every command is `proximosctl control.radio <command> [--radio lorawan|p2p] ...`. The
//! clap tree lives in the library, not in the binary, so whichever binary hosts
//! `proximosctl` mounts it as a dependency line rather than re-declaring the surface.
//!
//! Two properties here are contract rather than convenience, because the provisioning wizard
//! and the fleet updater consume them over SSH:
//!
//! - **Exit codes** map 1:1 to [`crate::Kind`] (§6's assigned table).
//! - **No secret ever reaches argv.** Key material enters only through `--keys-file <path>`
//!   or `--keys-stdin` (`system/design.md` §10, ledger #22). `tests/clap_tree.rs` walks the
//!   whole tree and fails if any secret-bearing flag takes a value.

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::error::{Component, Error, Kind, Transport};

/// The `control.radio` dispatch group, as an [`Args`] payload.
///
/// This is an `Args` rather than a `Parser` because the real invocation is
/// `proximosctl control.radio <command>`: the hosting binary mounts this as one subcommand
/// of its own tree. [`ControlRadioCli`] wraps it as a standalone parser so the tree can be
/// exercised here without depending on that binary — the dependency only runs the other way.
#[derive(Debug, Args)]
pub struct ControlRadio {
    /// Machine-readable output. The shapes are a golden baseline and evolve additively only.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// The dispatch group as a standalone parser.
///
/// Used by this crate's tests and by `--help` rendering; the shipped invocation goes through
/// the hosting binary's `control.radio` subcommand, which mounts [`ControlRadio`] directly.
/// Both therefore expose exactly the same tree.
#[derive(Debug, Parser)]
#[command(
    name = "control.radio",
    about = "Radio control: LoRaWAN and LoRa P2P node and gateway management",
    disable_help_subcommand = true
)]
pub struct ControlRadioCli {
    #[command(flatten)]
    pub inner: ControlRadio,
}

/// The `--radio` value. Optional when `radio.mode` names one transport, required under
/// `dual`, and irrelevant under `off` because everything is refused anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RadioArg {
    Lorawan,
    P2p,
}

impl From<RadioArg> for Transport {
    fn from(r: RadioArg) -> Self {
        match r {
            RadioArg::Lorawan => Transport::Lorawan,
            RadioArg::P2p => Transport::P2p,
        }
    }
}

/// Carried by every unified command.
#[derive(Debug, Args, Clone, Copy, Default)]
pub struct RadioSelector {
    /// Which transport to act on. Omit when `radio.mode` is unambiguous.
    #[arg(long, value_enum)]
    pub radio: Option<RadioArg>,
}

impl RadioSelector {
    pub fn transport(self) -> Option<Transport> {
        self.radio.map(Into::into)
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Converge this Hub's radio provisioning. Idempotent; safe to re-run at any time.
    Create(CreateArgs),
    /// Delete everything this component provisioned. The tenant is never deleted — it was
    /// resolved, not created.
    Wipe(ForceArgs),
    /// Delete only the enrolled devices; provisioning objects survive.
    Clear(ForceArgs),

    /// Register an external gateway (LoRaWAN), or a client Hub's gateway on the Server.
    GatewayAdd(GatewayAddArgs),
    /// Deregister a gateway.
    GatewayRemove(GatewayRemoveArgs),
    /// Summary of every known gateway — the same content as the `radio/gateways` topic.
    GatewayList(RadioSelector),
    /// The detailed registration record for one gateway.
    GatewayInfo(GatewayInfoArgs),
    /// Print the discovered gateway EUI without running `create`. Bench and HIL use.
    GatewayScan(RadioSelector),

    /// Register a node: enrolment in ChirpStack (LoRaWAN), a node-registry record (P2P).
    NodeAdd(NodeAddArgs),
    /// Remove a node. Synchronous on LoRaWAN; a pending detach on P2P.
    NodeRemove(NodeIdentArgs),
    /// Move a node to the other carrier (node_registry.md §4). Its identity, cache and history
    /// stay; the node itself must be switched by NFC or shell.
    NodeMove(NodeMoveArgs),
    /// List enrolled nodes.
    NodeList(RadioSelector),
    /// Read data the device already reported. Never triggers a downlink.
    NodeGet(NodeGetArgs),
    /// This component's own enrollment record for a node — local bookkeeping, not radio data.
    NodeInfo(NodeIdentArgs),
    /// Send something down to a node: a trigger, a structured command, or raw bytes.
    NodeSend(NodeSendArgs),

    /// Listen passively for unenrolled traffic. Diagnostics only — enrols nothing.
    NodeScanStart(RadioSelector),
    /// Stop passive listening.
    NodeScanStop(RadioSelector),

    /// Change a radio parameter the operator may move at runtime. P2P only.
    RadioSet(RadioSetArgs),

    /// Flash the Northbridge with the image's bundled firmware, or a hotfix. P2P only.
    NorthbridgeUpdate(NorthbridgeUpdateArgs),

    /// Expected-vs-actual per resource, plus token and bootstrap state.
    Status(RadioSelector),
}

impl Command {
    /// Whether this command takes `--radio`, and which arms it implements — the input to
    /// [`crate::mode::resolve`].
    pub fn radio_arity(&self) -> crate::mode::RadioArity {
        use crate::mode::RadioArity::*;
        match self {
            // Take `--radio` like a unified command (future-radio-ready), but only one arm
            // is implemented today; the other is InvalidArgument, not an *Unavailable.
            Command::NodeScanStart(_) | Command::NodeScanStop(_) | Command::RadioSet(_) => {
                UnifiedOneArmed(Transport::P2p)
            }
            // The Northbridge is fitted on every Hub; the idle task answers without P2P (#158).
            Command::NorthbridgeUpdate(_) => ModeIndependent(Transport::P2p),
            Command::GatewayScan(_) => UnifiedOneArmed(Transport::Lorawan),
            _ => Unified,
        }
    }

    /// The `--radio` value as given, for the commands that accept one.
    pub fn requested_transport(&self) -> Option<Transport> {
        match self {
            Command::Create(a) => a.radio.transport(),
            Command::Wipe(a) | Command::Clear(a) => a.radio.transport(),
            Command::GatewayAdd(a) => a.radio.transport(),
            Command::GatewayRemove(a) => a.radio.transport(),
            Command::GatewayInfo(a) => a.radio.transport(),
            Command::NodeAdd(a) => a.radio.transport(),
            Command::NodeRemove(a) | Command::NodeInfo(a) => a.radio.transport(),
            // Its own gate (`node_move::run`): the target is not a `--radio` choice.
            Command::NodeMove(_) => None,
            Command::NodeGet(a) => a.radio.transport(),
            Command::NodeSend(a) => a.radio.transport(),
            Command::RadioSet(a) => a.radio.transport(),
            Command::NorthbridgeUpdate(a) => a.radio.transport(),
            Command::GatewayList(s)
            | Command::GatewayScan(s)
            | Command::NodeList(s)
            | Command::NodeScanStart(s)
            | Command::NodeScanStop(s)
            | Command::Status(s) => s.transport(),
        }
    }

    /// The name used in the `"command"` field of the result envelope and in log lines.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Create(_) => "create",
            Command::Wipe(_) => "wipe",
            Command::Clear(_) => "clear",
            Command::GatewayAdd(_) => "gateway-add",
            Command::GatewayRemove(_) => "gateway-remove",
            Command::GatewayList(_) => "gateway-list",
            Command::GatewayInfo(_) => "gateway-info",
            Command::GatewayScan(_) => "gateway-scan",
            Command::NodeAdd(_) => "node-add",
            Command::NodeRemove(_) => "node-remove",
            Command::NodeMove(_) => "node-move",
            Command::NodeList(_) => "node-list",
            Command::NodeGet(_) => "node-get",
            Command::NodeInfo(_) => "node-info",
            Command::NodeSend(_) => "node-send",
            Command::NodeScanStart(_) => "node-scan-start",
            Command::NodeScanStop(_) => "node-scan-stop",
            Command::RadioSet(_) => "radio-set",
            Command::NorthbridgeUpdate(_) => "northbridge-update",
            Command::Status(_) => "status",
        }
    }

    /// Mutating commands return the §5 result envelope on success; reads return their own
    /// documented shape.
    pub fn is_mutating(&self) -> bool {
        matches!(
            self,
            Command::Create(_)
                | Command::Wipe(_)
                | Command::Clear(_)
                | Command::GatewayAdd(_)
                | Command::GatewayRemove(_)
                | Command::NodeAdd(_)
                | Command::NodeRemove(_)
                | Command::NodeSend(_)
                | Command::RadioSet(_)
                | Command::NorthbridgeUpdate(_)
        )
    }

    /// Argument constraints clap cannot express — or cannot be trusted alone for — checked
    /// before dispatch.
    ///
    /// Three live here. `node-send` takes **exactly one** of `--hex <payload>` or a `<type>`
    /// subcommand (§5): clap's `conflicts_with` cannot name a subcommand, and
    /// `args_conflicts_with_subcommands` would also sever `--radio`/`--serial` from `<type>`,
    /// which §5 requires to coexist. `radio-set` takes exactly one of `--spreading-factor` or
    /// `--reset`, and its range is re-checked here even though clap's `value_parser` already
    /// caps it, because [`crate::dispatch::run`] is a library entry point reached with a
    /// hand-built [`Command`] and no clap parse at all. `northbridge-update` takes `--file` and
    /// `--manifest` together, re-checked for the same reason. All produce the §6
    /// `InvalidArgument` (exit 2) every other usage error produces.
    pub fn validate(&self) -> crate::error::Result<()> {
        let usage = |msg: String| -> crate::error::Result<()> {
            Err(Error::new(
                Kind::InvalidArgument,
                None,
                Component::ControlSocket,
                msg,
            ))
        };
        if let Command::RadioSet(args) = self {
            match args.spreading_factor {
                Some(_) if args.reset => {
                    return usage(
                        "radio-set takes either --spreading-factor <7..12> or --reset, not \
                         both: which value to record would be undefined"
                            .to_string(),
                    );
                }
                None if !args.reset => {
                    return usage(
                        "radio-set needs either --spreading-factor <7..12> or --reset".to_string(),
                    );
                }
                Some(sf)
                    if !(crate::p2p::modem::SF_MIN..=crate::p2p::modem::SF_MAX).contains(&sf) =>
                {
                    return usage(format!(
                        "spreading factor {sf} is outside the {}..={} the modem accepts",
                        crate::p2p::modem::SF_MIN,
                        crate::p2p::modem::SF_MAX
                    ));
                }
                _ => {}
            }
        }
        if let Command::NorthbridgeUpdate(args) = self {
            if args.file.is_some() != args.manifest.is_some() {
                return usage(
                    "northbridge-update takes --file <northbridge.bin> and --manifest \
                     <northbridge.json> together: an image is checked against its manifest"
                        .to_string(),
                );
            }
            if args.timeout == 0 {
                return usage("northbridge-update --timeout must be at least 1 s".to_string());
            }
        }
        if let Command::NodeSend(args) = self {
            match (args.hex.is_some(), args.what.is_some()) {
                (true, true) => {
                    return usage(
                        "node-send takes either --hex <payload> or a <type>, not both: \
                         which one to send would be undefined"
                            .to_string(),
                    );
                }
                (false, false) => {
                    return usage(
                        "node-send needs either --hex <payload> or a <type> \
                         (get-config, get-info, set-config, cmd)"
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// `create --role`: the one cluster role a Hub can be given (plan/control/cluster.md §3). A
/// Server needs no role: a Hub with no role marker is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RoleArg {
    Client,
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    #[command(flatten)]
    pub radio: RadioSelector,

    /// Report the converge diff without writing anything. Same envelope, nothing changed on
    /// disk — a dry run is not a third response shape.
    #[arg(long)]
    pub dry_run: bool,

    /// Join this Hub to the Server Hub `--server-host` as a Client (plan/control/cluster.md
    /// §8): the CLI twin of the Manager app's BLE join, on the same path, and the Hub reboots
    /// into the client role. The broker password the Server minted for this Hub (the join
    /// bundle's `p`) is read from the first line of stdin, never from argv. Only on a Hub
    /// whose setup is not complete. A P2P Client is phase 2.
    #[arg(long, value_enum, requires_all = ["server_host", "ca_sha256"])]
    pub role: Option<RoleArg>,

    /// The Server Hub to join, by name (normally `<hostname>.local`; not an IP address). The
    /// join bundle's `h`.
    #[arg(long, requires = "role")]
    pub server_host: Option<String>,

    /// SHA-256 of the Server's CA as `GET /connection/ca` serves it, base64url without padding
    /// (43 characters). The join bundle's `f`.
    #[arg(long, requires = "role")]
    pub ca_sha256: Option<String>,

    /// Skip gateway EUI discovery and use this EUI. Bench and HIL fixtures.
    #[arg(long)]
    pub gateway_eui: Option<String>,
}

impl CreateArgs {
    /// `--role client`: a cluster join, not a converge.
    pub fn role_client(&self) -> bool {
        self.role == Some(RoleArg::Client)
    }
}

#[derive(Debug, Args, Clone, Copy)]
pub struct ForceArgs {
    #[command(flatten)]
    pub radio: RadioSelector,
    /// Required. These commands delete enrolled devices.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct GatewayAddArgs {
    #[command(flatten)]
    pub radio: RadioSelector,
    #[arg(long)]
    pub gateway_eui: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(Debug, Args)]
pub struct GatewayRemoveArgs {
    #[command(flatten)]
    pub radio: RadioSelector,
    #[arg(long)]
    pub gateway_eui: Option<String>,
}

#[derive(Debug, Args)]
pub struct GatewayInfoArgs {
    #[command(flatten)]
    pub radio: RadioSelector,
    #[arg(long)]
    pub gateway_eui: Option<String>,
}

/// `--serial` or `--dev-eui`, mutually exclusive, on both transports.
///
/// `--serial` is the primary operator-facing identifier; `--dev-eui` is the universal
/// alternative. Both resolve locally — no ChirpStack call and no radio traffic for the
/// lookup.
#[derive(Debug, Args)]
#[group(id = "node_ident", required = true, multiple = false)]
pub struct NodeIdent {
    #[arg(long, group = "node_ident")]
    pub serial: Option<String>,
    #[arg(long, group = "node_ident")]
    pub dev_eui: Option<String>,
}

#[derive(Debug, Args)]
pub struct NodeIdentArgs {
    #[command(flatten)]
    pub radio: RadioSelector,
    #[command(flatten)]
    pub ident: NodeIdent,
}

#[derive(Debug, Args)]
pub struct NodeMoveArgs {
    #[command(flatten)]
    pub ident: NodeIdent,
    /// The carrier the node moves to. Not gated by `radio.mode`: a node may be moved to a
    /// carrier this Hub has switched off, and the carrier picks it up when it is switched on.
    #[arg(long, value_enum)]
    pub to: RadioArg,
}

#[derive(Debug, Args)]
pub struct NodeAddArgs {
    #[command(flatten)]
    pub radio: RadioSelector,

    /// Which `device_profiles.{otaa|abp|p2p}.<name>` entry this device enrols under. The one
    /// flag required on every `node-add`, both transports. `default` if the fleet defines
    /// only one.
    #[arg(long)]
    pub profile: String,

    /// Operator-facing identity. Required on both transports — on P2P it is one third of the
    /// registration bundle, which is not optional (plan.md §5).
    #[arg(long)]
    pub serial: Option<String>,

    /// The device's hardware EUI. Same rule as `--serial`.
    #[arg(long)]
    pub dev_eui: Option<String>,

    #[arg(long)]
    pub name: Option<String>,

    /// LoRaWAN activation mode. No P2P equivalent — this is a LoRaWAN-only concept.
    #[arg(long, conflicts_with = "abp")]
    pub otaa: bool,
    #[arg(long, conflicts_with = "otaa")]
    pub abp: bool,

    /// Overrides `radio.join_eui`. OTAA only; no P2P equivalent.
    #[arg(long)]
    pub join_eui: Option<String>,

    /// Read key material from this file. **The path, never the key** — ledger #22.
    #[arg(long, conflicts_with = "keys_stdin")]
    pub keys_file: Option<std::path::PathBuf>,

    /// Read key material from stdin. A flag, so nothing lands in argv.
    #[arg(long, conflicts_with = "keys_file")]
    pub keys_stdin: bool,
}

/// What `node-get` reads back. Every one of these is cached data the device already
/// reported — reading never triggers a downlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum NodeGetType {
    /// The last received uplink/telemetry message.
    Message,
    /// Device info received over radio.
    DeviceInfo,
    /// The last received alarm.
    Alarm,
    /// Device config, settable keys.
    Config,
    /// Full config dump.
    FullConfig,
}

#[derive(Debug, Args)]
pub struct NodeGetArgs {
    #[command(flatten)]
    pub radio: RadioSelector,
    #[command(flatten)]
    pub ident: NodeIdent,
    #[arg(value_enum)]
    pub what: NodeGetType,
}

/// `radio-set --radio p2p` — the operator's runtime radio lever.
///
/// Only the spreading factor, and only on P2P. The northbridge has one radio, so SF is
/// network-wide by construction; on LoRaWAN the data rate is ADR's to choose, not an operator
/// setting, which is why this command is one-armed.
///
/// The value is recorded in `sessions.db`, never in the Hub config file: that file is rendered
/// (decision #4), so a lever that rewrote it would be lost on the next render.
#[derive(Debug, Args)]
pub struct RadioSetArgs {
    #[command(flatten)]
    pub radio: RadioSelector,

    /// The effective spreading factor, 7..=12. The running central switches its modem to it
    /// immediately; the nodes discover the change themselves.
    ///
    /// The range literal here is the CLI half of `p2p::modem::SF_MIN..=SF_MAX` — clap's range
    /// takes an `i64` bound, so it cannot name the constants directly. [`Command::validate`]
    /// re-checks against them, which is what the library entry point relies on.
    #[arg(long, value_parser = clap::value_parser!(u8).range(7..=12), conflicts_with = "reset")]
    pub spreading_factor: Option<u8>,

    /// Drop the override; `radio.p2p_radio.spreading_factor` stands again.
    #[arg(long)]
    pub reset: bool,
}

/// `northbridge-update --radio p2p` — flash the Northbridge now
/// (`plan/control/radio/northbridge_update.md` §3.4).
///
/// The P2P task does the work: it stops its central, flashes and starts it again. The command
/// writes a request for it and waits for the answer. Without `--file` the image is the one
/// the Hub image bundles, which the task already applies by itself at every start; the
/// command is for `--force` and for a hotfix.
#[derive(Debug, Args)]
pub struct NorthbridgeUpdateArgs {
    #[command(flatten)]
    pub radio: RadioSelector,

    /// A hotfix `northbridge.bin` instead of the bundled one. Needs `--manifest`.
    #[arg(long, value_name = "BIN", requires = "manifest")]
    pub file: Option<std::path::PathBuf>,

    /// The `northbridge.json` that describes `--file`.
    #[arg(long, value_name = "JSON", requires = "file")]
    pub manifest: Option<std::path::PathBuf>,

    /// Flash even when the Northbridge already runs this image, and past a spent retry
    /// budget.
    #[arg(long)]
    pub force: bool,

    /// Return once the request is written; `status --json` (`northbridge.update`) shows the
    /// outcome.
    #[arg(long)]
    pub no_wait: bool,

    /// How long to wait for the result, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 180)]
    pub timeout: u64,
}

#[derive(Debug, Args)]
pub struct NodeSendArgs {
    #[command(flatten)]
    pub radio: RadioSelector,
    #[command(flatten)]
    pub ident: NodeIdent,

    /// Raw downlink payload as hex. No length limit is exposed here — the transport
    /// fragments internally as needed.
    ///
    /// Mutually exclusive with `<type>`, enforced by [`Command::validate`] rather than by a
    /// clap `conflicts_with`: clap can only conflict args with args, and `what` is a
    /// subcommand. Naming it here compiled, then failed clap's own debug assertion at
    /// `Command`-build time — which meant every debug-build `node-send` aborted while release
    /// builds silently accepted both.
    #[arg(long, requires = "fport")]
    pub hex: Option<String>,

    /// The port to send a raw `--hex` payload on.
    ///
    /// **§5 never pinned a port for the raw form** — it says only "raw downlink, no length
    /// limit exposed here". A default would have to be invented, and every candidate is wrong
    /// for some caller: 85 is the command engine's port (§9.3), where a hand-written payload
    /// would be parsed as a `Command` protobuf, and 2/3 are uplink-only. So the flag is
    /// **required together with `--hex`** (clap `requires`, so the refusal is the same exit-2
    /// usage error every other malformed invocation gets) rather than defaulted. Carried to
    /// the plan-feedback set.
    ///
    /// It is deliberately *not* required for the structured `<type>` forms: those are fPort 85
    /// by protocol, so a port flag there would only offer a way to send them somewhere they
    /// cannot be understood.
    #[arg(long)]
    pub fport: Option<u8>,

    #[command(subcommand)]
    pub what: Option<NodeSendKind>,
}

#[derive(Debug, Subcommand)]
pub enum NodeSendKind {
    /// Ask the device to report its config. The answer arrives async; read it with
    /// `node-get config`.
    GetConfig {
        /// Start the answer at this page (0 = the first). The device streams the rest of a
        /// paged answer by itself; name a later page only to fill one that never arrived —
        /// the `command` document's `page_index`/`pages_received` say which (#96).
        #[arg(long, default_value_t = 0)]
        page: u32,
        /// Send under this sequence number instead of a fresh one. With `--page`, the missing
        /// pages of an incomplete answer then carry that answer's own `seq` (its `command`
        /// document names it) and complete it; a fresh `seq` would start a new answer instead.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        seq: Option<u32>,
    },
    /// Ask the device to report its info. Read it back with `node-get device-info`.
    GetInfo,
    /// Ask the device for the named keys only (`group.field`, repeated). One frame; the answer
    /// merges into what the Hub holds. Read it back with `node-get config`.
    GetParam {
        /// `group.field`, repeated. Named `params`, not `keys`: the CLI tree test keeps every
        /// argument whose name says "key" off argv, and these are config field names.
        #[arg(required = true, value_name = "GROUP.FIELD")]
        params: Vec<String>,
    },
    /// Ask the device for the small settings-info it sends after boot (intervals, history,
    /// sensors, 1-Wire slots) — one frame, unlike `get-config`. Read it back with
    /// `node-get config`, where it merges into what the Hub holds.
    GetSettings,
    /// Write device-config values, packed into as few downlinks as the Node's data rate allows
    /// (`save` on the last). `alarms.alarm_N=` with no value clears that slot.
    SetConfig {
        /// `group.field=value`, repeated.
        #[arg(required_unless_present = "replace_alarms")]
        assignments: Vec<String>,
        /// Clear every alarm slot before these values apply (firmware `SetParam.alarms_replace`),
        /// so the Node holds exactly the `alarms.alarm_N` given here.
        #[arg(long)]
        replace_alarms: bool,
        /// This write is the Node's whole pending configuration (config_sync.md §13 Q20b): an
        /// earlier `--supersede` write to the same Node still waiting in the queue is
        /// withdrawn for it. A frame already on the air stays and this one queues behind it.
        #[arg(long)]
        supersede: bool,
    },
    /// A structured command from the fPort-85 vocabulary.
    Cmd(NodeSendCmdArgs),
    /// Ask the Node for its device status: battery, downlink RSSI/SNR, TX power, uptime. The
    /// answer is published as `device_status`. P2P only (`0x91 DevStatusReq`); the Node must
    /// name the command in its Capabilities.
    DevStatus,
    /// Hold the Node's uplinks, alarms excepted, for this long (`0x91 Backoff`). `0` ends a
    /// running backoff. P2P only.
    Backoff {
        /// Seconds, 0..=3600.
        #[arg(long, value_parser = clap::value_parser!(u16).range(0..=3600))]
        seconds: u16,
    },
    /// Ask the Node to join again (`0x91 RejoinReq`). P2P only.
    Rejoin {
        /// `rediscover` finds the network again; `rekey` derives a new session key.
        #[arg(long, value_enum, default_value_t = RejoinKindArg::Rekey)]
        kind: RejoinKindArg,
    },
}

impl NodeSendKind {
    /// The `0x91` control command this kind sends, or `None` for an fPort-85 one
    /// (p2p_control_commands.md §2).
    pub fn control(&self) -> Option<crate::p2p::tower::control::ControlDownlink> {
        use crate::p2p::tower::control::{ControlDownlink, RejoinKind};
        match self {
            NodeSendKind::DevStatus => Some(ControlDownlink::DevStatusReq),
            NodeSendKind::Backoff { seconds } => {
                Some(ControlDownlink::Backoff { seconds: *seconds })
            }
            NodeSendKind::Rejoin { kind } => Some(ControlDownlink::RejoinReq {
                kind: match kind {
                    RejoinKindArg::Rediscover => RejoinKind::Rediscover,
                    RejoinKindArg::Rekey => RejoinKind::Rekey,
                },
            }),
            _ => None,
        }
    }
}

/// `node-send rejoin --kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RejoinKindArg {
    Rediscover,
    Rekey,
}

#[derive(Debug, Args)]
pub struct NodeSendCmdArgs {
    pub name: String,
    /// `history-replay` only: start of the window, unix seconds.
    #[arg(long)]
    pub from: Option<i64>,
    /// `history-replay` only: end of the window, unix seconds.
    #[arg(long)]
    pub to: Option<i64>,
    /// `reset-counters` only: which counter to clear. Repeatable, and **at least one is
    /// required** — the firmware reads an empty selection as "clear everything", so an
    /// operator's empty selection is refused before encoding (§9.7's mandatory guard) rather
    /// than silently clearing counters they did not choose.
    #[arg(long = "counter", value_enum)]
    pub counter: Vec<CounterName>,
}

/// The four counters `cmd reset-counters` can clear — `node-firmware-v1.4.0.md` §6 tag 10.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CounterName {
    HallLeft,
    HallRight,
    InputA,
    InputB,
}
