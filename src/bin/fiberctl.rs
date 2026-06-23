//! `fiberctl` — control CLI client for the FIBER application (#79).
//!
//! Thin client: parse args, build a [`Request`], round-trip it over the daemon's
//! control socket, print the response. The daemon (`fiber_app`) embeds the
//! server side (`fiber_app::libs::control::server`).

use std::collections::BTreeMap;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use fiber_app::libs::control::client;
use fiber_app::libs::control::protocol::{Command, LorawanSimpleCommand, Request, Response};

#[derive(Parser)]
#[command(
    name = "fiberctl",
    version,
    about = "Control CLI for the FIBER application (talks to the running fiber_app daemon)"
)]
struct Cli {
    /// Print the raw JSON response instead of a human summary.
    #[arg(long, global = true)]
    json: bool,
    /// Override the control socket path (default: FIBER_CONTROL_SOCKET or /run/fiber/control.sock).
    #[arg(long, global = true)]
    socket: Option<String>,
    #[command(subcommand)]
    cmd: TopCmd,
}

#[derive(Subcommand)]
enum TopCmd {
    /// Aggregate device status.
    Status,
    /// Inspect the effective configuration.
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// STICKER LoRaWAN control.
    Lorawan {
        #[command(subcommand)]
        action: LorawanCmd,
    },
    /// Read current DS18B20 / line sensor values.
    Sensors {
        #[command(subcommand)]
        action: SensorsCmd,
    },
    /// Battery / DC power status.
    Power,
}

#[derive(Subcommand)]
enum SensorsCmd {
    /// Print current sensor readings.
    Read,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Dump the whole config.
    Show,
    /// Read one dotted key, e.g. `system.app_version`.
    Get { key: String },
}

#[derive(Subcommand)]
enum LorawanCmd {
    /// Write config to a STICKER (SetParam). FIELDS are `group.field=value`.
    SetParam {
        dev_eui: String,
        /// One or more `group.field=value`, e.g. application.interval_report=600
        #[arg(required = true)]
        fields: Vec<String>,
        /// Persist + reboot the device after applying (destructive).
        #[arg(long)]
        save: bool,
        /// Required to run the destructive save/write path.
        #[arg(long)]
        force: bool,
    },
    /// Read config back from a STICKER (GetParam). KEYS are `group.field`.
    GetParam {
        dev_eui: String,
        #[arg(required = true)]
        keys: Vec<String>,
        /// Compare against desired `group.field=value` pairs and report mismatches.
        #[arg(long = "diff")]
        diff: Vec<String>,
    },
    /// Send a no-argument fPort-85 command.
    Send {
        dev_eui: String,
        command: SendCmd,
        /// Required for destructive commands (reboot, reset-counters).
        #[arg(long)]
        force: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum SendCmd {
    GetInfo,
    Reboot,
    ForceSend,
    ResetCounters,
    ClockSync,
}

impl From<SendCmd> for LorawanSimpleCommand {
    fn from(c: SendCmd) -> Self {
        match c {
            SendCmd::GetInfo => LorawanSimpleCommand::GetInfo,
            SendCmd::Reboot => LorawanSimpleCommand::Reboot,
            SendCmd::ForceSend => LorawanSimpleCommand::ForceSend,
            SendCmd::ResetCounters => LorawanSimpleCommand::ResetCounters,
            SendCmd::ClockSync => LorawanSimpleCommand::ClockSync,
        }
    }
}

fn parse_kv(items: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut m = BTreeMap::new();
    for it in items {
        let (k, v) = it
            .split_once('=')
            .ok_or_else(|| format!("expected key=value, got {it:?}"))?;
        m.insert(k.trim().to_string(), v.trim().to_string());
    }
    Ok(m)
}

fn build_command(cmd: TopCmd) -> Result<Command, String> {
    Ok(match cmd {
        TopCmd::Status => Command::Status,
        TopCmd::Config { action } => match action {
            ConfigCmd::Show => Command::ConfigShow,
            ConfigCmd::Get { key } => Command::ConfigGet { key },
        },
        TopCmd::Lorawan { action } => match action {
            LorawanCmd::SetParam { dev_eui, fields, save, force } => Command::LorawanSetParam {
                dev_eui,
                fields: parse_kv(&fields)?,
                save,
                force,
            },
            LorawanCmd::GetParam { dev_eui, keys, diff } => {
                let desired = if diff.is_empty() { None } else { Some(parse_kv(&diff)?) };
                Command::LorawanGetParam { dev_eui, keys, desired }
            }
            LorawanCmd::Send { dev_eui, command, force } => Command::LorawanSend {
                dev_eui,
                command: command.into(),
                force,
            },
        },
        TopCmd::Sensors { action } => match action {
            SensorsCmd::Read => Command::SensorsRead,
        },
        TopCmd::Power => Command::PowerStatus,
    })
}

fn print_response(resp: &Response, as_json: bool) {
    if as_json {
        println!("{}", serde_json::to_string_pretty(resp).unwrap_or_default());
        return;
    }
    if resp.ok {
        println!("{}", serde_json::to_string_pretty(&resp.data).unwrap_or_default());
    } else {
        eprintln!("error: {}", resp.error.as_deref().unwrap_or("unknown error"));
        if !resp.data.is_null() {
            eprintln!("{}", serde_json::to_string_pretty(&resp.data).unwrap_or_default());
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let as_json = cli.json;
    let socket = cli.socket.clone();

    let command = match build_command(cli.cmd) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    let req = Request::new(command);
    let result = match socket {
        Some(path) => client::send_to(&path, &req),
        None => client::send(&req),
    };

    match result {
        Ok(resp) => {
            print_response(&resp, as_json);
            if resp.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}
