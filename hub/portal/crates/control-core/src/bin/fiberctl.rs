//! `fiberctl` — control CLI client for the FIBER application (#79).
//!
//! Thin client: parse args, build a [`Request`], round-trip it over the daemon's
//! control socket, print the response. The daemon (`fiber_app`) embeds the
//! server side (`fiber_app::libs::control::server`).

use std::collections::BTreeMap;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use fiber_app::libs::control::client;
use fiber_app::libs::control::protocol::{
    Command, ConfigSetting, LorawanSimpleCommand, Request, Response,
};

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
    /// Node LoRaWAN control.
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
    /// MQTT broker connection.
    Mqtt {
        #[command(subcommand)]
        action: MqttCmd,
    },
    /// Factory test of the front panel and radios (before setup only).
    FactoryTest {
        #[command(subcommand)]
        action: FactoryTestCmd,
    },
    /// Reboot the Hub (audited, panel blanked), as the Portal does.
    Reboot {
        #[arg(long)]
        reason: Option<String>,
        /// Required: the Hub goes down.
        #[arg(long)]
        force: bool,
    },
    /// Deep standby, the Portal's power-off: dark, sensors off, back on when
    /// PoE returns or a front-panel button is held.
    Standby {
        #[arg(long)]
        reason: Option<String>,
        /// Required: the Hub stops monitoring.
        #[arg(long)]
        force: bool,
    },
    /// Shut the Hub down (audited, panel blanked, services stopped). Only the
    /// button on the back starts it again; PoE does not.
    Shutdown {
        #[arg(long)]
        reason: Option<String>,
        /// Required: the Hub goes down until someone presses the back button.
        #[arg(long)]
        force: bool,
    },
    /// Backlight off, Southbridge held in reset. Run by the system-shutdown hook
    /// on a power-off, after every service has stopped; needs no daemon.
    #[command(hide = true)]
    HwOff,
}

#[derive(Subcommand)]
enum FactoryTestCmd {
    /// Start the test (or attach to the running one) and print one JSON line
    /// per change until it ends; exits 0 on PASS.
    Run,
    /// Print the test state and the events after `--since`.
    Status {
        #[arg(long, default_value_t = 0)]
        since: u64,
    },
    /// Fail the current step.
    Fail {
        #[arg(long)]
        note: Option<String>,
    },
    /// End the test and hand the hardware back.
    Stop,
}

#[derive(Subcommand)]
enum SensorsCmd {
    /// Print current sensor readings.
    Read,
}

#[derive(Subcommand)]
enum MqttCmd {
    /// Show the MQTT connection state.
    Status,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Dump the whole config.
    Show,
    /// Read one dotted key, e.g. `system.app_version`.
    Get { key: String },
    /// Apply a persistent config change (atomic write + reload). Destructive.
    Set {
        #[command(subcommand)]
        setting: ConfigSetCmd,
        /// Required: config set persists to disk and reloads live. May appear
        /// before or after the setting (e.g. `config set --force ...` or
        /// `config set led-brightness 80 --force`).
        #[arg(long, global = true)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum ConfigSetCmd {
    DeviceLabel { label: String },
    Name { line: u8, name: String },
    Location { line: u8, location: String },
    LedBrightness { value: u8 },
    ScreenBrightness { value: u8 },
    BuzzerVolume { value: u8 },
    SystemInfoInterval { seconds: u64 },
}

impl From<ConfigSetCmd> for ConfigSetting {
    fn from(c: ConfigSetCmd) -> Self {
        match c {
            ConfigSetCmd::DeviceLabel { label } => ConfigSetting::DeviceLabel { label },
            ConfigSetCmd::Name { line, name } => ConfigSetting::SensorName { line, name },
            ConfigSetCmd::Location { line, location } => {
                ConfigSetting::SensorLocation { line, location }
            }
            ConfigSetCmd::LedBrightness { value } => ConfigSetting::LedBrightness { value },
            ConfigSetCmd::ScreenBrightness { value } => ConfigSetting::ScreenBrightness { value },
            ConfigSetCmd::BuzzerVolume { value } => ConfigSetting::BuzzerVolume { value },
            ConfigSetCmd::SystemInfoInterval { seconds } => {
                ConfigSetting::SystemInfoInterval { seconds }
            }
        }
    }
}

#[derive(Subcommand)]
enum LorawanCmd {
    /// Write config to a Node (SetParam). FIELDS are `group.field=value`.
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
    /// Read config back from a Node (GetParam). KEYS are `group.field`.
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
        /// Required for destructive commands (reboot, reset-counters,
        /// device-reset, factory-reset).
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
    /// Restore defaults, keeping identity and the LoRaWAN keys (proto id 8). The
    /// node stays joined; parameters and alarm rules are lost.
    DeviceReset,
    /// The NFC/shell-only factory reset (proto id 23). Always rejected over
    /// LoRaWAN with NOT_READY "transport not allowed" — present so that limit can
    /// be demonstrated on hardware rather than taken on trust.
    FactoryReset,
}

impl From<SendCmd> for LorawanSimpleCommand {
    fn from(c: SendCmd) -> Self {
        match c {
            SendCmd::GetInfo => LorawanSimpleCommand::GetInfo,
            SendCmd::Reboot => LorawanSimpleCommand::Reboot,
            SendCmd::ForceSend => LorawanSimpleCommand::ForceSend,
            SendCmd::ResetCounters => LorawanSimpleCommand::ResetCounters,
            SendCmd::ClockSync => LorawanSimpleCommand::ClockSync,
            SendCmd::DeviceReset => LorawanSimpleCommand::DeviceReset,
            SendCmd::FactoryReset => LorawanSimpleCommand::FactoryReset,
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
            ConfigCmd::Set { setting, force } => Command::ConfigSet {
                setting: setting.into(),
                force,
            },
        },
        TopCmd::Lorawan { action } => match action {
            LorawanCmd::SetParam {
                dev_eui,
                fields,
                save,
                force,
            } => Command::LorawanSetParam {
                dev_eui,
                fields: parse_kv(&fields)?,
                save,
                force,
            },
            LorawanCmd::GetParam {
                dev_eui,
                keys,
                diff,
            } => {
                let desired = if diff.is_empty() {
                    None
                } else {
                    Some(parse_kv(&diff)?)
                };
                Command::LorawanGetParam {
                    dev_eui,
                    keys,
                    desired,
                }
            }
            LorawanCmd::Send {
                dev_eui,
                command,
                force,
            } => Command::LorawanSend {
                dev_eui,
                command: command.into(),
                force,
            },
        },
        TopCmd::Sensors { action } => match action {
            SensorsCmd::Read => Command::SensorsRead,
        },
        TopCmd::Power => Command::PowerStatus,
        TopCmd::Mqtt { action } => match action {
            MqttCmd::Status => Command::MqttStatus,
        },
        TopCmd::FactoryTest { action } => match action {
            FactoryTestCmd::Run => return Err("factory-test run is not a single request".into()),
            FactoryTestCmd::Status { since } => Command::FactoryTestStatus { since },
            FactoryTestCmd::Fail { note } => Command::FactoryTestFail { note },
            FactoryTestCmd::Stop => Command::FactoryTestStop,
        },
        TopCmd::Reboot { reason, force } => Command::Reboot { reason, force },
        TopCmd::Standby { reason, force } => Command::Standby { reason, force },
        TopCmd::Shutdown { reason, force } => Command::Shutdown { reason, force },
        TopCmd::HwOff => return Err("hw-off runs locally, not over the socket".into()),
    })
}

fn print_response(resp: &Response, as_json: bool) {
    if as_json {
        println!("{}", serde_json::to_string_pretty(resp).unwrap_or_default());
        return;
    }
    if resp.ok {
        println!(
            "{}",
            serde_json::to_string_pretty(&resp.data).unwrap_or_default()
        );
    } else {
        eprintln!(
            "error: {}",
            resp.error.as_deref().unwrap_or("unknown error")
        );
        if !resp.data.is_null() {
            eprintln!(
                "{}",
                serde_json::to_string_pretty(&resp.data).unwrap_or_default()
            );
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let as_json = cli.json;
    let socket = cli.socket.clone();

    if matches!(
        cli.cmd,
        TopCmd::FactoryTest {
            action: FactoryTestCmd::Run
        }
    ) {
        return factory_test_run(socket.as_deref());
    }

    if matches!(cli.cmd, TopCmd::HwOff) {
        return match fiber_app::drivers::board_off::hold_board_off() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: hw-off: {e}");
                ExitCode::FAILURE
            }
        };
    }

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

fn round_trip(socket: Option<&str>, command: Command) -> Result<Response, String> {
    let req = Request::new(command);
    match socket {
        Some(path) => client::send_to(path, &req),
        None => client::send(&req),
    }
    .map_err(|e| e.to_string())
}

/// `factory-test run`: the provisioning app reads stdout over SSH. Lines are
/// `{"type":"state",...}` whenever the state changes, `{"type":"event",...}`
/// per test event and a last `{"type":"end","passed":...}`. Polling is the
/// test's heartbeat: when this process dies the test aborts on its own.
fn factory_test_run(socket: Option<&str>) -> ExitCode {
    use serde_json::{json, Value};

    let emit = |v: Value| {
        println!("{v}");
    };
    let fail = |e: String| {
        emit(json!({"type": "error", "error": e}));
        ExitCode::from(2)
    };

    match round_trip(socket, Command::FactoryTestStart) {
        Ok(r) if r.ok => {}
        // A rerun after a dropped connection attaches to the running test.
        Ok(r)
            if r.error
                .as_deref()
                .is_some_and(|e| e.contains("already running")) => {}
        Ok(r) => return fail(r.error.unwrap_or_else(|| "refused".into())),
        Err(e) => return fail(e),
    }

    let mut since = 0u64;
    let mut last_state: Option<Value> = None;
    loop {
        let r = match round_trip(socket, Command::FactoryTestStatus { since }) {
            Ok(r) if r.ok => r,
            Ok(r) => return fail(r.error.unwrap_or_default()),
            Err(e) => return fail(e),
        };
        let mut data = r.data;
        if let Some(events) = data.get_mut("events").map(Value::take) {
            for ev in events.as_array().into_iter().flatten() {
                if let Some(seq) = ev.get("seq").and_then(Value::as_u64) {
                    since = since.max(seq);
                }
                let mut line = json!({"type": "event"});
                if let (Some(line), Some(ev)) = (line.as_object_mut(), ev.as_object()) {
                    line.extend(ev.clone());
                }
                emit(line);
            }
        }
        let active = data.get("active").and_then(Value::as_bool).unwrap_or(false);
        let passed = data.get("passed").and_then(Value::as_bool).unwrap_or(false);
        if let Some(obj) = data.as_object_mut() {
            obj.remove("events");
            obj.remove("elapsed_s");
            obj.insert("type".into(), json!("state"));
        }
        if last_state.as_ref() != Some(&data) {
            emit(data.clone());
            last_state = Some(data);
        }
        if !active {
            emit(json!({"type": "end", "passed": passed}));
            return if passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}
