//! `radio.mode` at runtime — the signed `radio-set-mode` and BLE FB1B
//! (`plan/control/radio/node_registry.md` §11 amended 2026-09-27 R7, §12).
//!
//! The twin of `control.radio set-mode`, and like the signed `node-add` it is
//! `control_radio`'s own logic run here: [`control_radio::setmode`] validates the mode against
//! the hardware table and plans the restart. What this process adds is the two things only it
//! may do:
//!
//! - **the write** — `radio.mode` is a key of this agent's own config, so it goes through
//!   [`ConfigApplier::apply_radio_mode`] like every other Hub-side setting, with a backup and
//!   the Hub audit row `SET_RADIO_MODE`;
//! - **the restart** — `fiber.service` runs as root, which is also how it reboots the Hub, so
//!   it asks systemd itself. After the ack's grace window, on a thread of its own, so the
//!   SUCCESS ack is on the wire before the Portal that is waiting for it restarts. A refused
//!   restart falls back to the ordinary audited reboot path.
//!
//! **One apply path, two surfaces.** [`apply`] is what the signed verb ([`run`]) and the
//! provisioning choice over BLE FB1B (`ble_host::radio_mode`) both call, so the hardware
//! refusal, the write with its audit row and the restart cannot differ between them; only the
//! audit row's `origin` does. [`status`] is FB1B's read.
//!
//! Nothing here deletes anything: the registry, caches, sessions and `net_id` stay where the
//! radio store keeps them (`control-radio/tests/set_mode.rs`).

use std::sync::Arc;

use control_radio::setmode::{self, Applied, Plan};
use control_radio::{Hardware, Mode};

use crate::libs::config_applier::ConfigApplier;

/// What a mode change reads from the machine and does to it: the rendered mode, the card, and
/// the restart. A trait so a test can run the whole apply path without restarting the machine
/// it runs on; [`ThisHub`] is the real one.
pub trait Machine: Send + Sync {
    /// The mode the Hub was rendered with — what it runs until the restart re-renders.
    fn rendered_mode(&self) -> Result<Mode, String>;
    /// Whether the RAK5146 is fitted (`setmode::probe_hardware`, the one detection).
    fn hardware(&self) -> Hardware;
    /// Carry out a plan whose write has landed.
    fn restart(&self, plan: Plan) -> Result<(), String>;
}

/// The Hub this process runs on.
pub struct ThisHub;

impl Machine for ThisHub {
    fn rendered_mode(&self) -> Result<Mode, String> {
        control_radio::dispatch::load_config()
            .map(|c| c.mode)
            .map_err(|e| format!("this Hub has no v2 radio configuration to change ({e})"))
    }

    fn hardware(&self) -> Hardware {
        setmode::probe_hardware()
    }

    fn restart(&self, plan: Plan) -> Result<(), String> {
        spawn_restart(plan)
    }
}

/// What an accepted mode change did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Changed {
    /// The mode changed: it is written, and the restart is queued.
    Restarting { from: Mode, to: Mode },
    /// The Hub already runs the mode, but nothing had chosen it — the hardware default or the
    /// overlay picked it. The choice is written now and nothing restarts: it is what keeps the
    /// mode when the default would differ (a card that does not enumerate one boot, §12).
    Recorded(Mode),
    /// The Hub runs the mode and it is already the chosen one: nothing written, nothing
    /// restarted.
    Unchanged(Mode),
}

impl Changed {
    /// One line for the surface that asked (FB1B's `message`). Short on purpose: FB1B serves it
    /// beside the whole state in one PDU, and at the 185-byte MTU phones negotiate these fit
    /// whole (`ble_host::radio_mode`'s test pins it).
    pub fn message(&self) -> String {
        match self {
            Changed::Restarting { from, to } => format!(
                "{} (was {}); the radio restarts",
                to.as_str(),
                from.as_str()
            ),
            Changed::Recorded(mode) => format!("{} chosen; it already runs", mode.as_str()),
            Changed::Unchanged(mode) => format!("already {}; nothing changed", mode.as_str()),
        }
    }
}

/// Execute a confirmed signed `radio-set-mode` on this Hub. `Err` is the command response's
/// `error_message`.
pub fn run(mode: &str, config_applier: &Option<Arc<ConfigApplier>>) -> Result<(), String> {
    let applier = config_applier
        .as_ref()
        .ok_or_else(|| "Config applier not initialized".to_string())?;
    apply(mode, applier, "signed-mqtt", &ThisHub).map(|_| ())
}

/// The one apply path of a mode change, for every surface that makes one. `origin` names the
/// surface in the audit row (`signed-mqtt`, `ble`).
pub fn apply(
    mode: &str,
    applier: &ConfigApplier,
    origin: &str,
    machine: &dyn Machine,
) -> Result<Changed, String> {
    run_with(
        mode,
        machine.rendered_mode(),
        machine.hardware(),
        applier,
        origin,
        |plan| machine.restart(plan),
    )
}

/// [`apply`] with its inputs spelled out: the rendered mode, the hardware, and what to do with
/// the plan once the write has landed. The order is the contract — validate, then write, then
/// restart — so a refusal leaves the file and systemd untouched, and a failed write restarts
/// nothing.
pub fn run_with(
    mode: &str,
    current: Result<Mode, String>,
    hardware: Hardware,
    applier: &ConfigApplier,
    origin: &str,
    restart: impl FnOnce(Plan) -> Result<(), String>,
) -> Result<Changed, String> {
    let to = setmode::parse_mode(mode).map_err(|e| e.message().to_string())?;
    let from = current?;
    let plan = setmode::plan(from, to, hardware).map_err(|e| e.message().to_string())?;
    if plan.is_noop() {
        if applier.chosen_radio_mode()?.as_deref() == Some(to.as_str()) {
            eprintln!(
                "[radio_mode] radio.mode is already {}; nothing to change",
                to.as_str()
            );
            return Ok(Changed::Unchanged(to));
        }
        write(applier, to, from, origin)?;
        return Ok(Changed::Recorded(to));
    }
    write(applier, to, from, origin)?;
    restart(plan)?;
    Ok(Changed::Restarting { from, to })
}

fn write(applier: &ConfigApplier, to: Mode, from: Mode, origin: &str) -> Result<(), String> {
    let result = applier.apply_radio_mode(to.as_str(), from.as_str(), origin);
    if result.success {
        Ok(())
    } else {
        Err(result
            .error_message
            .unwrap_or_else(|| "cannot record the radio mode".to_string()))
    }
}

/// FB1B's read: what this Hub runs, whether that was chosen, and what it may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The chosen mode when there is one — which is also what the Hub restarts into right after
    /// a change — else the rendered one: the overlay's, or the hardware default.
    pub mode: Mode,
    /// `radio.mode` is in `fiber.config.yaml`: provisioning or `radio-set-mode` chose it.
    pub chosen: bool,
    /// The RAK5146 is detected.
    pub lorawan_card: bool,
    /// The hardware table of plan.md §1 — what [`apply`] accepts on this Hub.
    pub allowed: Vec<Mode>,
    /// The mode this Hub runs when nothing chose one (§12).
    pub default: Mode,
}

/// The mode this Hub runs, or restarts into: the chosen `radio.mode` when there is one, else
/// `rendered` — [`Status::mode`]'s rule, without the hardware probe. `Err` as [`status`].
pub fn effective_mode(
    applier: &ConfigApplier,
    rendered: impl FnOnce() -> Result<Mode, String>,
) -> Result<Mode, String> {
    match applier.chosen_radio_mode()? {
        Some(m) => setmode::parse_mode(&m).map_err(|e| e.message().to_string()),
        None => rendered(),
    }
}

/// [`Status`] of this Hub. `Err` is "this Hub cannot answer": an unreadable config file, a
/// chosen mode that is not one of the four, or no mode at all (nothing chosen and no rendered
/// config).
pub fn status(applier: &ConfigApplier, machine: &dyn Machine) -> Result<Status, String> {
    let chosen = applier
        .chosen_radio_mode()?
        .map(|m| setmode::parse_mode(&m).map_err(|e| e.message().to_string()))
        .transpose()?;
    let hardware = machine.hardware();
    let mode = match chosen {
        Some(mode) => mode,
        None => machine.rendered_mode()?,
    };
    Ok(Status {
        mode,
        chosen: chosen.is_some(),
        lorawan_card: hardware == Hardware::NorthbridgeAndConcentrator,
        allowed: hardware.valid_modes().to_vec(),
        default: setmode::default_mode(hardware),
    })
}

/// Run the plan after [`crate::libs::system_control::TEARDOWN_GRACE`], on a thread of its own.
///
/// The ack is only queued when the command returns (see `system_control::spawn_teardown`), and
/// the plan restarts the Portal that is waiting for it. A refused systemd call reboots through
/// `spawn_teardown`, the same path a signed reboot takes: the boot path renders the new mode.
fn spawn_restart(plan: Plan) -> Result<(), String> {
    std::thread::Builder::new()
        .name("radio-set-mode".to_string())
        .spawn(move || {
            std::thread::sleep(crate::libs::system_control::TEARDOWN_GRACE);
            match setmode::execute(&plan, &setmode::Systemctl) {
                Applied::Unchanged => {}
                Applied::Restarting => eprintln!(
                    "[radio_mode] radio.mode {} → {}: restart queued (stopped {:?}, restarting {:?})",
                    plan.from.as_str(),
                    plan.to.as_str(),
                    plan.stop,
                    plan.restart
                ),
                Applied::RebootRequired { refused } => {
                    eprintln!(
                        "[radio_mode] systemd refused the restart ({refused}); rebooting to apply \
                         radio.mode {}",
                        plan.to.as_str()
                    );
                    if let Err(e) = crate::libs::system_control::spawn_teardown("reboot") {
                        eprintln!("[radio_mode] the reboot fallback failed too: {e}");
                    }
                }
            }
        })
        .map(|_| ())
        .map_err(|e| format!("cannot start the radio restart: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "power:\n  poll_ms: 1000\nsystem:\n  system_info_sec: 60\n";

    fn applier() -> (tempfile::TempDir, ConfigApplier) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("fiber.config.yaml"), CONFIG).expect("config");
        let applier = ConfigApplier::new(dir.path()).expect("applier");
        (dir, applier)
    }

    fn written(dir: &tempfile::TempDir) -> serde_yaml::Value {
        serde_yaml::from_str(
            &std::fs::read_to_string(dir.path().join("fiber.config.yaml")).expect("read"),
        )
        .expect("yaml")
    }

    /// How many times the applier wrote the file: it backs the file up before every write.
    fn writes(dir: &tempfile::TempDir) -> usize {
        std::fs::read_dir(dir.path().join(".backups"))
            .map(|d| d.count())
            .unwrap_or(0)
    }

    /// A machine that is not this one: a rendered mode, a card or none, and restarts recorded.
    struct FakeMachine {
        rendered: Result<Mode, String>,
        hardware: Hardware,
        restarts: std::sync::Mutex<Vec<Plan>>,
    }

    impl FakeMachine {
        fn new(rendered: Mode, hardware: Hardware) -> Self {
            Self {
                rendered: Ok(rendered),
                hardware,
                restarts: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl Machine for FakeMachine {
        fn rendered_mode(&self) -> Result<Mode, String> {
            self.rendered.clone()
        }
        fn hardware(&self) -> Hardware {
            self.hardware
        }
        fn restart(&self, plan: Plan) -> Result<(), String> {
            self.restarts.lock().expect("lock").push(plan);
            Ok(())
        }
    }

    /// Validate, write, restart — and the plan handed on is the one for this transition.
    #[test]
    fn a_change_writes_the_mode_then_hands_the_plan_on() {
        let (dir, applier) = applier();
        let mut planned = None;
        let changed = run_with(
            "p2p",
            Ok(Mode::Lorawan),
            Hardware::NorthbridgeAndConcentrator,
            &applier,
            "test",
            |plan| {
                planned = Some(plan);
                Ok(())
            },
        )
        .expect("changed");
        assert_eq!(
            changed,
            Changed::Restarting {
                from: Mode::Lorawan,
                to: Mode::P2p
            }
        );
        let doc = written(&dir);
        assert_eq!(doc["radio"]["mode"], "p2p");
        assert_eq!(
            doc["system"]["system_info_sec"], 60,
            "the rest of the file rides through"
        );
        let plan = planned.expect("restart was asked for");
        assert_eq!((plan.from, plan.to), (Mode::Lorawan, Mode::P2p));
        assert!(plan.stop.contains(&"chirpstack.service"), "{plan:?}");
        assert_eq!(plan.restart[0], "proximos-render.service");
    }

    /// §11: `lorawan`/`dual` are refused without a RAK5146, before anything is written.
    #[test]
    fn no_concentrator_refuses_lorawan_before_the_write() {
        let (dir, applier) = applier();
        for mode in ["lorawan", "dual"] {
            let err = run_with(
                mode,
                Ok(Mode::P2p),
                Hardware::NorthbridgeOnly,
                &applier,
                "test",
                |_| panic!("no restart for a refused mode"),
            )
            .expect_err("refused");
            assert!(err.contains("RAK5146"), "{err}");
        }
        assert!(written(&dir).get("radio").is_none(), "nothing was written");
    }

    #[test]
    fn a_mode_that_is_not_one_of_the_four_is_refused() {
        let (dir, applier) = applier();
        let err = run_with(
            "both",
            Ok(Mode::P2p),
            Hardware::NorthbridgeAndConcentrator,
            &applier,
            "test",
            |_| panic!("no restart"),
        )
        .expect_err("refused");
        assert!(err.contains("both"), "{err}");
        assert!(written(&dir).get("radio").is_none());
    }

    /// The mode the Hub already runs restarts nothing. When nothing had chosen it (the hardware
    /// default or the overlay did), it is recorded as the choice, so a later boot with another
    /// default keeps it (§12); asked again, nothing is written at all.
    #[test]
    fn the_current_mode_again_restarts_nothing_and_is_recorded_once() {
        let (dir, applier) = applier();
        let changed = run_with(
            "p2p",
            Ok(Mode::P2p),
            Hardware::NorthbridgeOnly,
            &applier,
            "test",
            |_| panic!("no restart for the same mode"),
        )
        .expect("a success");
        assert_eq!(changed, Changed::Recorded(Mode::P2p));
        assert_eq!(
            written(&dir)["radio"]["mode"],
            "p2p",
            "the choice is recorded"
        );
        let after_first = writes(&dir);
        assert_eq!(after_first, 1);

        let changed = run_with(
            "p2p",
            Ok(Mode::P2p),
            Hardware::NorthbridgeOnly,
            &applier,
            "test",
            |_| panic!("no restart for the same mode"),
        )
        .expect("a quiet success");
        assert_eq!(changed, Changed::Unchanged(Mode::P2p));
        assert_eq!(
            writes(&dir),
            after_first,
            "nothing was written the second time"
        );
    }

    /// A Hub without a rendered radio config has no mode to change from.
    #[test]
    fn no_rendered_config_is_refused() {
        let (dir, applier) = applier();
        let err = run_with(
            "p2p",
            Err("this Hub has no v2 radio configuration to change".to_string()),
            Hardware::NorthbridgeAndConcentrator,
            &applier,
            "test",
            |_| panic!("no restart"),
        )
        .expect_err("refused");
        assert!(err.contains("no v2 radio configuration"), "{err}");
        assert!(written(&dir).get("radio").is_none());
    }

    /// A write that fails restarts nothing, so the Hub never runs a mode it did not record.
    #[test]
    fn a_failed_write_restarts_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let applier = ConfigApplier::new(dir.path()).expect("applier");
        // No fiber.config.yaml at all.
        let err = run_with(
            "p2p",
            Ok(Mode::Lorawan),
            Hardware::NorthbridgeAndConcentrator,
            &applier,
            "test",
            |_| panic!("no restart after a failed write"),
        )
        .expect_err("the write failed");
        assert!(err.contains("not found"), "{err}");
    }

    /// A `radio:` section with other keys keeps them; only `mode` changes.
    #[test]
    fn only_the_mode_key_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("fiber.config.yaml"),
            "radio:\n  mode: lorawan\n  future_key: 7\n",
        )
        .expect("config");
        let applier = ConfigApplier::new(dir.path()).expect("applier");
        run_with(
            "dual",
            Ok(Mode::Lorawan),
            Hardware::NorthbridgeAndConcentrator,
            &applier,
            "test",
            |_| Ok(()),
        )
        .expect("changed");
        let doc = written(&dir);
        assert_eq!(doc["radio"]["mode"], "dual");
        assert_eq!(doc["radio"]["future_key"], 7);
    }

    /// [`apply`] takes the rendered mode, the card and the restart from the machine.
    #[test]
    fn apply_asks_the_machine_and_restarts_through_it() {
        let (dir, applier) = applier();
        let hub = FakeMachine::new(Mode::P2p, Hardware::NorthbridgeAndConcentrator);
        let changed = apply("lorawan", &applier, "ble", &hub).expect("changed");
        assert_eq!(
            changed,
            Changed::Restarting {
                from: Mode::P2p,
                to: Mode::Lorawan
            }
        );
        assert_eq!(written(&dir)["radio"]["mode"], "lorawan");
        let restarts = hub.restarts.lock().expect("lock").clone();
        assert_eq!(restarts.len(), 1);
        assert!(restarts[0].restart.contains(&"chirpstack.service"));

        let bare = FakeMachine::new(Mode::P2p, Hardware::NorthbridgeOnly);
        let (dir, applier) = self::applier();
        apply("lorawan", &applier, "ble", &bare).expect_err("no card");
        assert!(written(&dir).get("radio").is_none());
        assert!(bare.restarts.lock().expect("lock").is_empty());
    }

    /// FB1B's read: nothing chosen reports the rendered mode; a choice reports itself, also
    /// right after a change, before the restart re-rendered.
    #[test]
    fn status_reports_the_choice_the_card_and_the_table() {
        let (_dir, applier) = applier();
        let card = FakeMachine::new(Mode::Lorawan, Hardware::NorthbridgeAndConcentrator);
        assert_eq!(
            status(&applier, &card).expect("status"),
            Status {
                mode: Mode::Lorawan,
                chosen: false,
                lorawan_card: true,
                allowed: vec![Mode::Off, Mode::P2p, Mode::Lorawan, Mode::Dual],
                default: Mode::Lorawan,
            }
        );

        apply("p2p", &applier, "ble", &card).expect("changed");
        let s = status(&applier, &card).expect("status");
        assert_eq!((s.mode, s.chosen), (Mode::P2p, true), "{s:?}");

        let (_dir, applier) = self::applier();
        let bare = FakeMachine::new(Mode::P2p, Hardware::NorthbridgeOnly);
        assert_eq!(
            status(&applier, &bare).expect("status"),
            Status {
                mode: Mode::P2p,
                chosen: false,
                lorawan_card: false,
                allowed: vec![Mode::Off, Mode::P2p],
                default: Mode::P2p,
            }
        );
    }

    /// "Cannot answer" is an error, never a made-up mode: no config file, a chosen mode that is
    /// not one of the four, or nothing chosen and nothing rendered.
    #[test]
    fn status_that_cannot_be_answered_is_an_error() {
        let card = FakeMachine::new(Mode::Lorawan, Hardware::NorthbridgeAndConcentrator);
        let dir = tempfile::tempdir().expect("tempdir");
        let applier = ConfigApplier::new(dir.path()).expect("applier");
        status(&applier, &card).expect_err("no config file");

        std::fs::write(
            dir.path().join("fiber.config.yaml"),
            "radio:\n  mode: both\n",
        )
        .expect("config");
        let err = status(&applier, &card).expect_err("not a mode");
        assert!(err.contains("both"), "{err}");

        std::fs::write(dir.path().join("fiber.config.yaml"), CONFIG).expect("config");
        let unrendered = FakeMachine {
            rendered: Err("no rendered radio config".to_string()),
            ..FakeMachine::new(Mode::Off, Hardware::NorthbridgeOnly)
        };
        status(&applier, &unrendered).expect_err("no mode anywhere");
    }
}
