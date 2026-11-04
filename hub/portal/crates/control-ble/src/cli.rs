//! The published command surface — `plan/control/ble/plan.md` §5.
//!
//! Every command is `proximosctl control.ble <command> [--role central|peripheral] [...]`. The clap
//! tree lives in the library, not in the binary, so whichever binary hosts `proximosctl` mounts it
//! as a dependency line rather than re-declaring the surface.
//!
//! Three properties here are contract rather than convenience:
//!
//! - **Exit codes** map 1:1 to [`crate::Kind`] (§6's assigned table).
//! - **Every peripheral command carries a `phone-` prefix and every central one a `beacon-`**
//!   prefix (§5): one adapter, two roles, and `--role` alone does not read well at a glance in a
//!   shell history or a runbook. `create`/`status` stay unprefixed because they are the two
//!   commands that genuinely exist on both roles — and they are the only two that take `--role`.
//! - **No credential ever reaches argv.** The session token is *minted*, never accepted: there is
//!   no `--pin` and no `--token` anywhere in this tree. `tests/clap_tree.rs` walks it and fails if
//!   one appears.
//!
//! `Command::name()` is also the signed-MQTT `command_type` (§9.1's one rule, shared with the radio
//! half): the wire verb *is* `beacon-add`, `beacon-monitoring`, … The v1 `*_eye_*` strings exist
//! only in plan.md §9.8's historical map, and v2 never accepts or emits one.

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::{Sensor, TX_POWER_ALLOWED};
use crate::error::{Component, Error, Kind, Result, Role};
use crate::mac::MacAddr;
use crate::mode::RoleArity;

/// The `control.ble` dispatch group, as an [`Args`] payload.
#[derive(Debug, Args)]
pub struct ControlBle {
    /// Machine-readable output. The shapes are a golden baseline and evolve additively only.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// The dispatch group as a standalone parser, for this crate's tests, the harness and `--help`.
/// The shipped invocation goes through `proximosctl control.ble`, which mounts [`ControlBle`]
/// directly — so both expose exactly the same tree.
#[derive(Debug, Parser)]
#[command(
    name = "control.ble",
    about = "BLE control: Beacon telemetry and provisioning, and the phone-facing FB00 service",
    disable_help_subcommand = true
)]
pub struct ControlBleCli {
    #[command(flatten)]
    pub inner: ControlBle,
}

/// The `--role` value. Optional when `ble.mode` names one role, required under `dual`, and
/// irrelevant under `off` because everything is refused anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RoleArg {
    Central,
    Peripheral,
}

impl From<RoleArg> for Role {
    fn from(r: RoleArg) -> Self {
        match r {
            RoleArg::Central => Role::Central,
            RoleArg::Peripheral => Role::Peripheral,
        }
    }
}

/// Carried only by the two commands that exist on both roles.
#[derive(Debug, Args, Clone, Copy, Default)]
pub struct RoleSelector {
    /// Which role to act on. Omit when `ble.mode` is unambiguous.
    #[arg(long, value_enum)]
    pub role: Option<RoleArg>,
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    #[command(flatten)]
    pub role: RoleSelector,
    /// Report what would be initialised without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct MacArgs {
    /// The Beacon's address. Any case, with or without separators.
    #[arg(long)]
    pub mac: String,
}

#[derive(Debug, Args)]
pub struct BeaconHistoryArgs {
    /// The Beacon's address. Any case, with or without separators.
    #[arg(long)]
    pub mac: String,
    /// Window start, unix seconds. Without it the download resumes after the newest stored record.
    #[arg(long, allow_negative_numbers = true)]
    pub from: Option<i64>,
    /// Window end, unix seconds. Records after it are dropped.
    #[arg(long, allow_negative_numbers = true)]
    pub to: Option<i64>,
    /// Job id echoed on every `eye/history` message. Without it the Hub makes one.
    #[arg(long)]
    pub job: Option<String>,
}

#[derive(Debug, Args)]
pub struct BeaconAddArgs {
    /// The Beacon's address. Any case, with or without separators.
    #[arg(long)]
    pub mac: String,
    /// A human label for the Beacon, at most 64 characters.
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(Debug, Args)]
pub struct BeaconListArgs {
    /// Answer exactly as FB0F would for this page. Omit to list every enrolled Beacon.
    #[arg(long)]
    pub page: Option<u16>,
    /// The ATT MTU to size the page against. The default is iOS's typical negotiated value.
    #[arg(long, default_value_t = DEFAULT_MTU)]
    pub mtu: u16,
}

/// iOS's typical negotiated ATT_MTU — the value `beacon-list --page` sizes against by default.
pub const DEFAULT_MTU: u16 = 185;
/// The ATT minimum. A link below this cannot carry one list item, so it is a usage error.
pub const MIN_MTU: u16 = 23;

#[derive(Debug, Args)]
pub struct BeaconMonitoringArgs {
    /// Turn Beacon monitoring (`eye.enabled`) on or off. The loop starts or idles at runtime — not
    /// only the flag.
    #[arg(long, action = clap::ArgAction::Set)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BeaconGetType {
    /// The last decoded advertisement.
    Reading,
    /// `discovered` / `provisioning` / `is_en12830`.
    Axes,
    /// The last downloaded EN12830 archive segment, from cache.
    History,
}

impl BeaconGetType {
    pub const fn as_str(self) -> &'static str {
        match self {
            BeaconGetType::Reading => "reading",
            BeaconGetType::Axes => "axes",
            BeaconGetType::History => "history",
        }
    }
}

#[derive(Debug, Args)]
pub struct BeaconGetArgs {
    #[arg(long)]
    pub mac: String,
    /// What to read. Never dials — `beacon-get` returns what has already arrived.
    #[arg(value_enum)]
    pub what: BeaconGetType,
}

#[derive(Debug, Args)]
pub struct BeaconSendArgs {
    #[command(subcommand)]
    pub what: BeaconSendKind,
}

#[derive(Debug, Subcommand)]
pub enum BeaconSendKind {
    /// Ad hoc **partial** override: writes only the fields given, leaving the rest of the Beacon's
    /// current configuration untouched. Never re-applies the full default profile.
    Config(SendConfigArgs),
}

#[derive(Debug, Args)]
pub struct SendConfigArgs {
    #[arg(long)]
    pub mac: String,
    /// 1..=10 seconds. Written to the characteristic as little-endian milliseconds.
    #[arg(long)]
    pub advertising_interval_s: Option<u16>,
    /// One of -14, -11, -8, -5, -2, 2, 4, 8.
    #[arg(long)]
    pub tx_power_dbm: Option<i8>,
    /// Comma-separated subset of temperature, humidity, magnet, movement.
    #[arg(long)]
    pub sensors: Option<String>,
}

impl SendConfigArgs {
    /// A partial override with nothing to write is a usage error, not a no-op dial.
    pub fn is_empty(&self) -> bool {
        self.advertising_interval_s.is_none()
            && self.tx_power_dbm.is_none()
            && self.sensors.is_none()
    }
}

#[derive(Debug, Args)]
pub struct BeaconRecordingArgs {
    #[arg(long)]
    pub mac: String,
    /// The archive logging interval. The Beacon supports 1, 5 and 15 minutes; `0` stops recording.
    #[arg(long)]
    pub interval_min: u16,
}

#[derive(Debug, Args)]
pub struct KnownBeaconsArgs {
    /// Comma-separated fleet allowlist. **Empty is legal** and means "listen for none".
    #[arg(long)]
    pub macs: String,
}

impl KnownBeaconsArgs {
    /// The non-empty entries, in order. An empty string is an empty list, not one blank entry.
    pub fn entries(&self) -> Vec<&str> {
        self.macs
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    }
}

#[derive(Debug, Args)]
pub struct SessionStartArgs {
    /// Override `ble.peripheral.session_idle_timeout_s` for this session.
    #[arg(long)]
    pub idle_timeout_s: Option<u32>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize this component's `/data` state and probe the adapter.
    #[command(name = "create")]
    Create(CreateArgs),
    /// Adapter state, per-role provisioning markers, Beacon counts, the capability flag, the live
    /// phone session.
    #[command(name = "status")]
    Status(RoleSelector),
    /// Begin passive listening for unenrolled Beacons. Diagnostics; enrolls nothing.
    #[command(name = "beacon-scan-start")]
    BeaconScanStart,
    /// Stop passive listening.
    #[command(name = "beacon-scan-stop")]
    BeaconScanStop,
    /// The current `discovered` set — Beacons heard but not registered.
    #[command(name = "beacon-scan-list")]
    BeaconScanList,
    /// Enrolled Beacons. With `--page`, answers exactly as FB0F would at that MTU.
    #[command(name = "beacon-list")]
    BeaconList(BeaconListArgs),
    /// Enroll a Beacon. Turns monitoring on if it was off, and says so in the result.
    #[command(name = "beacon-add")]
    BeaconAdd(BeaconAddArgs),
    /// Remove a Beacon. Identical in effect to FB10 and to the signed `beacon-remove`.
    #[command(name = "beacon-remove")]
    BeaconRemove(MacArgs),
    /// This component's own enrollment record for one Beacon.
    #[command(name = "beacon-info")]
    BeaconInfo(MacArgs),
    /// Read cached data the Beacon already reported. Never dials.
    #[command(name = "beacon-get")]
    BeaconGet(BeaconGetArgs),
    /// Force a **full** provisioning attempt now — every field of `ble.central.beacon_profile`.
    #[command(name = "beacon-provision")]
    BeaconProvision(MacArgs),
    /// Write a partial configuration override to one Beacon.
    #[command(name = "beacon-send")]
    BeaconSend(BeaconSendArgs),
    /// Probe whether a Beacon is an EN12830 recorder. Behind the capability flag.
    #[command(name = "beacon-detect")]
    BeaconDetect(MacArgs),
    /// Set the archive logging interval. Behind the capability flag.
    #[command(name = "beacon-recording")]
    BeaconRecording(BeaconRecordingArgs),
    /// Back-fill a recorder Beacon's archive now. Behind the capability flag.
    #[command(name = "beacon-history")]
    BeaconHistory(BeaconHistoryArgs),
    /// Turn Beacon monitoring on or off — the explicit twin of enrollment's auto-enable.
    #[command(name = "beacon-monitoring")]
    BeaconMonitoring(BeaconMonitoringArgs),
    /// Validate a fleet allowlist. Reports what the daemon would accept; cannot install it.
    #[command(name = "known-beacons")]
    KnownBeacons(KnownBeaconsArgs),
    /// Mint a provisioning session. Prints the QR payload **without** the token.
    #[command(name = "phone-session-start")]
    PhoneSessionStart(SessionStartArgs),
    /// End the live session, de-authenticating any peer.
    #[command(name = "phone-session-stop")]
    PhoneSessionStop,
    /// Whether a central is authenticated, and its address.
    #[command(name = "phone-session-status")]
    PhoneSessionStatus,
    /// Bring the FB00 service up without restarting the component.
    #[command(name = "phone-advertise-start")]
    PhoneAdvertiseStart,
    /// Take the FB00 service down without stopping the component.
    #[command(name = "phone-advertise-stop")]
    PhoneAdvertiseStop,
}

impl Command {
    /// `Both` for the two commands that span roles; `Fixed` for everything else, whose role is in
    /// its own name and therefore cannot be contradicted by a flag.
    pub const fn role_arity(&self) -> RoleArity {
        match self {
            Command::Create(_) | Command::Status(_) => RoleArity::Both,
            Command::PhoneSessionStart(_)
            | Command::PhoneSessionStop
            | Command::PhoneSessionStatus
            | Command::PhoneAdvertiseStart
            | Command::PhoneAdvertiseStop => RoleArity::Fixed(Role::Peripheral),
            _ => RoleArity::Fixed(Role::Central),
        }
    }

    /// What the operator actually named, for the refusal to report back.
    pub fn requested_role(&self) -> Option<Role> {
        match self {
            Command::Create(a) => a.role.role.map(Into::into),
            Command::Status(s) => s.role.map(Into::into),
            _ => None,
        }
    }

    /// The kebab-case wire name — also the signed-MQTT `command_type` (§9.1, §9.8).
    pub const fn name(&self) -> &'static str {
        match self {
            Command::Create(_) => "create",
            Command::Status(_) => "status",
            Command::BeaconScanStart => "beacon-scan-start",
            Command::BeaconScanStop => "beacon-scan-stop",
            Command::BeaconScanList => "beacon-scan-list",
            Command::BeaconList(_) => "beacon-list",
            Command::BeaconAdd(_) => "beacon-add",
            Command::BeaconRemove(_) => "beacon-remove",
            Command::BeaconInfo(_) => "beacon-info",
            Command::BeaconGet(_) => "beacon-get",
            Command::BeaconProvision(_) => "beacon-provision",
            Command::BeaconSend(_) => "beacon-send",
            Command::BeaconDetect(_) => "beacon-detect",
            Command::BeaconRecording(_) => "beacon-recording",
            Command::BeaconHistory(_) => "beacon-history",
            Command::BeaconMonitoring(_) => "beacon-monitoring",
            Command::KnownBeacons(_) => "known-beacons",
            Command::PhoneSessionStart(_) => "phone-session-start",
            Command::PhoneSessionStop => "phone-session-stop",
            Command::PhoneSessionStatus => "phone-session-status",
            Command::PhoneAdvertiseStart => "phone-advertise-start",
            Command::PhoneAdvertiseStop => "phone-advertise-stop",
        }
    }

    /// Whether this command changes state — every one of these carries a result envelope (§5).
    /// `known-beacons` is **not** mutating: it validates and reports; installing is the signed
    /// twin's seam.
    pub const fn is_mutating(&self) -> bool {
        matches!(
            self,
            Command::Create(_)
                | Command::BeaconScanStart
                | Command::BeaconScanStop
                | Command::BeaconAdd(_)
                | Command::BeaconRemove(_)
                | Command::BeaconMonitoring(_)
                | Command::BeaconProvision(_)
                | Command::BeaconSend(_)
                | Command::BeaconRecording(_)
                | Command::BeaconHistory(_)
                | Command::BeaconDetect(_)
                | Command::PhoneSessionStart(_)
                | Command::PhoneSessionStop
                | Command::PhoneAdvertiseStart
                | Command::PhoneAdvertiseStop
        )
    }

    /// The `--mac` this command addresses, if any — normalized, so every surface downstream sees
    /// the canonical form.
    pub fn addressed_mac(&self) -> Option<Result<MacAddr>> {
        let raw = match self {
            Command::BeaconAdd(a) => &a.mac,
            Command::BeaconRemove(a)
            | Command::BeaconInfo(a)
            | Command::BeaconProvision(a)
            | Command::BeaconDetect(a) => &a.mac,
            Command::BeaconHistory(a) => &a.mac,
            Command::BeaconGet(a) => &a.mac,
            Command::BeaconRecording(a) => &a.mac,
            Command::BeaconSend(a) => match &a.what {
                BeaconSendKind::Config(c) => &c.mac,
            },
            _ => return None,
        };
        Some(MacAddr::parse(raw))
    }

    /// Argument-shape checks clap cannot express. Runs **after** the mode gate, so a refusal on an
    /// `off` Hub is `OffMode` rather than a complaint about a flag.
    pub fn validate(&self) -> Result<()> {
        if let Some(mac) = self.addressed_mac() {
            mac?;
        }
        match self {
            Command::BeaconHistory(a) => {
                crate::central::history::HistoryRequest::new(a.from, a.to, a.job.clone(), 0)
                    .map_err(|e| usage(e.message()))?;
            }
            Command::BeaconSend(a) => match &a.what {
                BeaconSendKind::Config(c) => {
                    if c.is_empty() {
                        return Err(usage(
                            "beacon-send config needs at least one of --advertising-interval-s, \
                             --tx-power-dbm or --sensors: a partial override with nothing to write \
                             would dial the beacon and change nothing",
                        ));
                    }
                    if let Some(s) = &c.sensors {
                        parse_sensors(s)?;
                    }
                    if let Some(i) = c.advertising_interval_s {
                        if !(1..=10).contains(&i) {
                            return Err(usage(format!(
                                "--advertising-interval-s {i}: allowed 1..=10 seconds"
                            )));
                        }
                    }
                    if let Some(t) = c.tx_power_dbm {
                        if !TX_POWER_ALLOWED.contains(&t) {
                            return Err(usage(format!(
                                "--tx-power-dbm {t}: allowed {TX_POWER_ALLOWED:?}"
                            )));
                        }
                    }
                }
            },
            Command::BeaconRecording(a) => {
                if !RECORDING_INTERVALS_MIN.contains(&a.interval_min) {
                    return Err(usage(format!(
                        "--interval-min {}: the beacon supports 1, 5 and 15 minutes, or 0 to stop \
                         recording. A value outside that set is refused rather than snapped to a \
                         neighbour — an operator who asked for a five-minute archive and silently \
                         got fifteen has a compliance problem and no way to know",
                        a.interval_min
                    )));
                }
            }
            Command::BeaconList(a) => {
                if a.mtu < MIN_MTU {
                    return Err(usage(format!(
                        "--mtu {}: the ATT minimum is {MIN_MTU}",
                        a.mtu
                    )));
                }
            }
            Command::KnownBeacons(a) => {
                // An empty list is legal; a malformed entry is not, and the message names it.
                for entry in a.entries() {
                    MacAddr::parse(entry)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// `0` stops recording; otherwise the beacon accepts only these (beacon-btsmp1.md §3).
pub const RECORDING_INTERVALS_MIN: [u16; 4] = [0, 1, 5, 15];

fn usage(message: impl Into<String>) -> Error {
    Error::new(
        Kind::InvalidArgument,
        None,
        Component::BeaconRegistry,
        message,
    )
}

/// Parse a `--sensors` CSV into the documented set. Rejects an unknown name and a blank entry —
/// `temperature,` is a typo, not a one-element list.
pub fn parse_sensors(csv: &str) -> Result<Vec<Sensor>> {
    let mut out = Vec::new();
    for raw in csv.split(',') {
        let name = raw.trim();
        let found = Sensor::ALL.iter().copied().find(|s| s.as_str() == name);
        match found {
            Some(s) if !out.contains(&s) => out.push(s),
            Some(_) => {}
            None => {
                let allowed: Vec<&str> = Sensor::ALL.iter().map(|s| s.as_str()).collect();
                return Err(usage(format!(
                    "--sensors {name:?} is not a sensor: allowed {}",
                    allowed.join(", ")
                )));
            }
        }
    }
    if out.is_empty() {
        return Err(usage(
            "--sensors was given with no sensor names: omit the flag instead of passing an empty \
             set, which would write a mask of zero and silence the beacon",
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sensors_dedups_and_preserves_order() {
        assert_eq!(
            parse_sensors("humidity,temperature,humidity").expect("parses"),
            vec![Sensor::Humidity, Sensor::Temperature]
        );
    }

    #[test]
    fn parse_sensors_refuses_an_empty_set_rather_than_writing_a_zero_mask() {
        // A mask of zero would tell the beacon to report nothing — a silent way to lose a fleet.
        assert!(parse_sensors("").is_err());
        assert!(parse_sensors(" ").is_err());
        assert!(parse_sensors(",").is_err());
    }

    #[test]
    fn known_beacons_entries_treats_an_empty_string_as_an_empty_list() {
        let a = KnownBeaconsArgs {
            macs: String::new(),
        };
        assert!(a.entries().is_empty());
        let a = KnownBeaconsArgs {
            macs: "AA:BB:CC:DD:EE:FF, 00:11:22:33:44:55 ,".to_string(),
        };
        assert_eq!(
            a.entries(),
            vec!["AA:BB:CC:DD:EE:FF", "00:11:22:33:44:55"],
            "blank entries from a trailing comma are dropped, not parsed"
        );
    }

    #[test]
    fn send_config_is_empty_only_when_no_value_flag_is_given() {
        let none = SendConfigArgs {
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            advertising_interval_s: None,
            tx_power_dbm: None,
            sensors: None,
        };
        assert!(none.is_empty());
        let one = SendConfigArgs {
            tx_power_dbm: Some(4),
            ..none
        };
        assert!(!one.is_empty());
    }

    #[test]
    fn the_recording_interval_set_includes_the_stop_value() {
        assert!(RECORDING_INTERVALS_MIN.contains(&0), "0 stops recording");
        for min in [1u16, 5, 15] {
            assert!(RECORDING_INTERVALS_MIN.contains(&min));
        }
        for min in [2u16, 3, 7, 10, 20, 30, 60] {
            assert!(!RECORDING_INTERVALS_MIN.contains(&min));
        }
    }
}
