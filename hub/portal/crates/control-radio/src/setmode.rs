//! `control.radio set-mode` — changing the Hub's `radio.mode` at runtime
//! (`plan/control/radio/node_registry.md` §11, amended 2026-09-27 R7).
//!
//! `radio.mode` stopped being a factory-only choice once every Node lived in the one registry:
//! leaving a carrier no longer loses anyone's identity or keys. What is left of a mode change
//! is three steps, and this module owns the two that are the radio's:
//!
//! 1. **Validate** — the mode is one of the four, and the hardware can run it (plan.md §1's
//!    table: `lorawan`/`dual` need a RAK5146). [`plan`].
//! 2. *Persist* — `radio.mode` in `fiber.service`'s own config. That file is the agent's, so the
//!    agent writes it (`ConfigApplier::apply_radio_mode` in control-core); nothing here does.
//! 3. **Restart** — re-render and restart the units whose behaviour the mode decides:
//!    [`Plan::stop`] first, then [`Plan::restart`] in one systemd call. [`execute`].
//!
//! **Nothing is deleted.** Neither step here opens a file under the radio store: the registry,
//! the per-Node caches, `sessions.db`, the P2P `net_id` and the ChirpStack tokens are left
//! exactly as they are, and the arms find them again when they start under the new mode.
//! `tests/set_mode.rs` holds the whole store tree byte for byte across a mode round trip.

use std::path::Path;

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::diagnostics::{
    self, Card, CARD_TTY, CARD_USB_ID, UNIT_CHIRPSTACK, UNIT_CONCENTRATORD, UNIT_FORWARDER,
};
use crate::mode::{Hardware, Mode};

/// The renderer's oneshot. Restarting it re-renders every file under `/run/proximos`, and the
/// ChirpStack pair appears or disappears with `radio.mode`.
pub const UNIT_RENDER: &str = "proximos-render.service";
/// `control.radio create` at boot: converges whatever arms the new mode enables.
pub const UNIT_RADIO_SETUP: &str = "radio-setup.service";
/// The resident radio tasks, spawned per mode at start.
pub const UNIT_CONTROL: &str = "proximos-control.service";
/// The Portal. Its in-process `control.radio` seam reads `radio.mode` once, at start, so it
/// restarts with the radio or it keeps routing by the old mode.
pub const UNIT_PORTAL: &str = "fiber-portal.service";
/// ChirpStack's credential bootstrap. Condition-skipped once its tokens exist, so listing it
/// costs nothing on a Hub that ran LoRaWAN before and is what a first LoRaWAN start needs.
pub const UNIT_CHIRPSTACK_BOOTSTRAP: &str = "chirpstack-bootstrap.service";

/// Parse a mode as the wire and the CLI spell it: `off`, `lorawan`, `p2p`, `dual`.
pub fn parse_mode(text: &str) -> Result<Mode> {
    Mode::ALL
        .into_iter()
        .find(|m| m.as_str() == text.trim())
        .ok_or_else(|| {
            Error::new(
                Kind::InvalidArgument,
                None,
                Component::RadioMode,
                format!("{text:?} is not a radio mode: expected off, lorawan, p2p or dual"),
            )
        })
}

/// Whether a mode runs the LoRaWAN network server (ChirpStack, the concentrator, the forwarder).
/// The same question the renderer gates the ChirpStack pair on.
const fn runs_lorawan(mode: Mode) -> bool {
    matches!(mode, Mode::Lorawan | Mode::Dual)
}

/// The RAK5146 as the LoRaWAN diagnostics see it: the concentrator's tty, with the USB IDs of
/// the card's bridge (`99-chirpstack-concentratord.rules`). The tty alone is any CDC-ACM device
/// someone plugged in; IDs that cannot be read are not taken as a card either.
pub fn hardware_of(card: &Card) -> Hardware {
    if card.present && card.usb_id.as_deref() == Some(CARD_USB_ID) {
        Hardware::NorthbridgeAndConcentrator
    } else {
        Hardware::NorthbridgeOnly
    }
}

/// [`hardware_of`] this Hub. Blocking (a stat and two sysfs reads).
///
/// The one detection of the RAK5146: `radio-set-mode`'s hardware check, BLE FB1B's
/// `lorawan_card` and the renderer's default mode ([`default_mode`], through `proximosctl
/// render`) all ask this, so the three cannot disagree about whether the card is there.
pub fn probe_hardware() -> Hardware {
    hardware_of(&diagnostics::probe_card(
        Path::new(CARD_TTY),
        Path::new("/sys/class/tty"),
    ))
}

/// The mode a Hub runs when nothing chose one (`plan/control/radio/node_registry.md` §12):
/// `lorawan` with the RAK5146, `p2p` without it.
///
/// "Chose" is `radio.mode` in `fiber.service`'s config (the provisioning choice over BLE FB1B,
/// or `radio-set-mode`) or in the device overlay; either wins over this. A card that did not
/// enumerate at boot reads as `p2p` until a choice is made, which is what the provisioning
/// choice is for.
pub const fn default_mode(hardware: Hardware) -> Mode {
    match hardware {
        Hardware::NorthbridgeAndConcentrator => Mode::Lorawan,
        Hardware::NorthbridgeOnly => Mode::P2p,
    }
}

/// What one mode change does to systemd. Computed from the two modes alone, so every
/// transition is testable as data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Plan {
    pub from: Mode,
    pub to: Mode,
    /// Stopped first, and waited for: the LoRaWAN units when the new mode runs no LoRaWAN.
    /// Their `ConditionPathExists=` would skip them on the restart anyway; stopping them first
    /// keeps them out of the restart's cascade altogether.
    pub stop: Vec<&'static str>,
    /// Restarted in **one** `systemctl restart --no-block`, so systemd orders the jobs by the
    /// static unit graph: the re-render before everything that reads it, ChirpStack before
    /// `radio-setup`, `radio-setup` before `proximos-control`.
    pub restart: Vec<&'static str>,
}

impl Plan {
    /// The mode asked for is the one this Hub runs: nothing to write, nothing to restart.
    pub fn is_noop(&self) -> bool {
        self.from == self.to
    }
}

/// Validate a change from `from` (the rendered mode) to `to`, and plan its restart.
///
/// Refuses, as `InvalidArgument` from the `radio-mode` component, a mode this hardware cannot
/// run. The provisioning tool enforced the table once at flash time (plan.md §1); a runtime
/// change must enforce it again, since nothing else stands between an operator and a LoRaWAN
/// Hub with no concentrator.
pub fn plan(from: Mode, to: Mode, hardware: Hardware) -> Result<Plan> {
    if !hardware.valid_modes().contains(&to) {
        return Err(Error::new(
            Kind::InvalidArgument,
            Transport::Lorawan,
            Component::RadioMode,
            format!(
                "radio mode {} needs the RAK5146 LoRaWAN concentrator, and this Hub has none \
                 ({CARD_TTY} with USB IDs {CARD_USB_ID} is missing; a fitted card that did not \
                 enumerate needs a cold power cycle)",
                to.as_str()
            ),
        ));
    }
    if from == to {
        return Ok(Plan {
            from,
            to,
            stop: Vec::new(),
            restart: Vec::new(),
        });
    }
    let stop = if runs_lorawan(from) && !runs_lorawan(to) {
        vec![UNIT_FORWARDER, UNIT_CONCENTRATORD, UNIT_CHIRPSTACK]
    } else {
        Vec::new()
    };
    let mut restart = vec![UNIT_RENDER];
    if runs_lorawan(to) {
        restart.extend([
            UNIT_CHIRPSTACK_BOOTSTRAP,
            UNIT_CHIRPSTACK,
            UNIT_CONCENTRATORD,
        ]);
    }
    restart.extend([UNIT_RADIO_SETUP, UNIT_CONTROL, UNIT_PORTAL]);
    Ok(Plan {
        from,
        to,
        stop,
        restart,
    })
}

/// The one call this module makes into systemd, behind a trait so a plan's execution is
/// testable without a systemd — and without restarting the machine the tests run on.
pub trait Systemd {
    /// Run `systemctl <args>`. `Err` carries why it was refused: it could not be run, or it
    /// exited non-zero.
    fn systemctl(&self, args: &[&str]) -> std::result::Result<(), String>;
}

/// The real `systemctl`. The caller must be allowed to manage these units: `fiber.service`
/// runs as root, the same way it already reboots the Hub.
pub struct Systemctl;

impl Systemd for Systemctl {
    fn systemctl(&self, args: &[&str]) -> std::result::Result<(), String> {
        let out = std::process::Command::new("systemctl")
            .args(args)
            .output()
            .map_err(|e| format!("cannot run systemctl: {e}"))?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(format!(
            "systemctl {} exited with {}: {}",
            args.join(" "),
            out.status,
            stderr.trim()
        ))
    }
}

/// What [`execute`] got systemd to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// The mode did not change; systemd was not asked for anything.
    Unchanged,
    /// The stops are done and the restart jobs are queued.
    Restarting,
    /// systemd refused a call. The fallback is a reboot, which renders the new mode on the boot
    /// path; the caller reboots the way it always does (control-core blanks the display first).
    RebootRequired { refused: String },
}

/// Run a [`Plan`]: the stops, waited for, then the one restart call, queued.
///
/// `--no-block` on the restart, so "refused" means what the design means by it — systemd would
/// not queue the jobs (a unit it does not know, a transaction it will not take) — and not that
/// a unit came up degraded afterwards. A degraded `radio-setup` is reported where it always is,
/// in `radio/gateways` and `status`, not by rebooting a Hub that did change its mode.
pub fn execute(plan: &Plan, systemd: &dyn Systemd) -> Applied {
    if plan.is_noop() {
        return Applied::Unchanged;
    }
    if !plan.stop.is_empty() {
        let mut args = vec!["stop"];
        args.extend(plan.stop.iter().copied());
        if let Err(refused) = systemd.systemctl(&args) {
            return Applied::RebootRequired { refused };
        }
    }
    let mut args = vec!["restart", "--no-block"];
    args.extend(plan.restart.iter().copied());
    match systemd.systemctl(&args) {
        Ok(()) => Applied::Restarting,
        Err(refused) => Applied::RebootRequired { refused },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_spellings_parse_and_nothing_else_does() {
        for mode in Mode::ALL {
            assert_eq!(parse_mode(mode.as_str()).expect("parses"), mode);
        }
        assert_eq!(parse_mode(" p2p\n").expect("trimmed"), Mode::P2p);
        for bad in ["", "LoRaWAN", "lorawan-and-p2p", "both"] {
            let err = parse_mode(bad).expect_err(bad);
            assert_eq!(err.kind(), Kind::InvalidArgument, "{bad}");
        }
    }

    #[test]
    fn a_card_is_the_tty_with_the_rak5146_usb_ids() {
        let card = |present, usb_id: Option<&str>| Card {
            present,
            device: CARD_TTY.to_string(),
            usb_id: usb_id.map(str::to_string),
        };
        assert_eq!(
            hardware_of(&card(true, Some(CARD_USB_ID))),
            Hardware::NorthbridgeAndConcentrator
        );
        assert_eq!(hardware_of(&card(false, None)), Hardware::NorthbridgeOnly);
        assert_eq!(
            hardware_of(&card(true, Some("1a86:7523"))),
            Hardware::NorthbridgeOnly,
            "another CDC-ACM device is not a concentrator"
        );
        assert_eq!(
            hardware_of(&card(true, None)),
            Hardware::NorthbridgeOnly,
            "IDs that cannot be read are not proof of a card"
        );
    }

    /// §12: `lorawan` with the card, `p2p` without — and the default is always a mode the
    /// hardware can run, so a Hub nobody configured never renders a mode it would refuse.
    #[test]
    fn the_default_follows_the_card_and_is_always_allowed() {
        assert_eq!(
            default_mode(Hardware::NorthbridgeAndConcentrator),
            Mode::Lorawan
        );
        assert_eq!(default_mode(Hardware::NorthbridgeOnly), Mode::P2p);
        for hw in [
            Hardware::NorthbridgeOnly,
            Hardware::NorthbridgeAndConcentrator,
        ] {
            assert!(hw.valid_modes().contains(&default_mode(hw)), "{hw:?}");
        }
    }

    #[test]
    fn lorawan_and_dual_are_refused_without_a_concentrator() {
        for to in [Mode::Lorawan, Mode::Dual] {
            let err = plan(Mode::P2p, to, Hardware::NorthbridgeOnly).expect_err("refused");
            assert_eq!(err.kind(), Kind::InvalidArgument);
            assert_eq!(err.component(), Component::RadioMode);
            assert!(err.to_string().contains("RAK5146"), "{err}");
        }
        for to in [Mode::Off, Mode::P2p] {
            plan(Mode::Lorawan, to, Hardware::NorthbridgeOnly).expect("no card needed");
        }
        for to in Mode::ALL {
            plan(Mode::P2p, to, Hardware::NorthbridgeAndConcentrator).expect("a card runs all");
        }
    }

    /// The restart plan, per old → new mode.
    #[test]
    fn the_plan_follows_the_lorawan_server_in_and_out() {
        let hw = Hardware::NorthbridgeAndConcentrator;
        let base = [UNIT_RADIO_SETUP, UNIT_CONTROL, UNIT_PORTAL];
        let with_lorawan = [
            UNIT_RENDER,
            UNIT_CHIRPSTACK_BOOTSTRAP,
            UNIT_CHIRPSTACK,
            UNIT_CONCENTRATORD,
            UNIT_RADIO_SETUP,
            UNIT_CONTROL,
            UNIT_PORTAL,
        ];
        let lorawan_units = [UNIT_FORWARDER, UNIT_CONCENTRATORD, UNIT_CHIRPSTACK];
        let without: Vec<&str> = std::iter::once(UNIT_RENDER).chain(base).collect();

        for from in Mode::ALL {
            for to in Mode::ALL {
                let p = plan(from, to, hw).expect("valid");
                assert_eq!((p.from, p.to), (from, to));
                if from == to {
                    assert!(p.is_noop() && p.stop.is_empty() && p.restart.is_empty());
                    continue;
                }
                assert!(!p.is_noop());
                assert_eq!(
                    p.restart[0], UNIT_RENDER,
                    "{from:?}→{to:?}: re-render first"
                );
                if runs_lorawan(to) {
                    assert_eq!(p.restart, with_lorawan, "{from:?}→{to:?}");
                } else {
                    assert_eq!(p.restart, without, "{from:?}→{to:?}");
                }
                if runs_lorawan(from) && !runs_lorawan(to) {
                    assert_eq!(p.stop, lorawan_units, "{from:?}→{to:?}");
                } else {
                    assert!(p.stop.is_empty(), "{from:?}→{to:?}");
                }
            }
        }
    }

    /// Records every call; refuses the n-th one when asked to.
    struct FakeSystemd {
        calls: std::sync::Mutex<Vec<Vec<String>>>,
        refuse_call: Option<usize>,
    }

    impl FakeSystemd {
        fn new(refuse_call: Option<usize>) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                refuse_call,
            }
        }
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().expect("lock").clone()
        }
    }

    impl Systemd for FakeSystemd {
        fn systemctl(&self, args: &[&str]) -> std::result::Result<(), String> {
            let mut calls = self.calls.lock().expect("lock");
            calls.push(args.iter().map(|a| a.to_string()).collect());
            if self.refuse_call == Some(calls.len() - 1) {
                return Err("Unit radio-setup.service is masked.".to_string());
            }
            Ok(())
        }
    }

    #[test]
    fn execute_stops_then_restarts_in_one_non_blocking_call() {
        let p = plan(Mode::Dual, Mode::P2p, Hardware::NorthbridgeAndConcentrator).expect("plan");
        let systemd = FakeSystemd::new(None);
        assert_eq!(execute(&p, &systemd), Applied::Restarting);
        let calls = systemd.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0][0], "stop");
        assert_eq!(calls[0][1..], p.stop.map_to_strings());
        assert_eq!(calls[1][..2], ["restart", "--no-block"]);
        assert_eq!(calls[1][2..], p.restart.map_to_strings());

        // Entering LoRaWAN stops nothing.
        let p = plan(Mode::P2p, Mode::Dual, Hardware::NorthbridgeAndConcentrator).expect("plan");
        let systemd = FakeSystemd::new(None);
        assert_eq!(execute(&p, &systemd), Applied::Restarting);
        assert_eq!(systemd.calls().len(), 1);
    }

    #[test]
    fn the_same_mode_asks_systemd_for_nothing() {
        let p = plan(Mode::P2p, Mode::P2p, Hardware::NorthbridgeOnly).expect("plan");
        let systemd = FakeSystemd::new(Some(0));
        assert_eq!(execute(&p, &systemd), Applied::Unchanged);
        assert!(systemd.calls().is_empty());
    }

    /// §11: "a reboot is the fallback when a unit refuses the restart".
    #[test]
    fn a_refused_call_asks_for_the_reboot_fallback() {
        let hw = Hardware::NorthbridgeAndConcentrator;
        // The restart refused.
        let p = plan(Mode::P2p, Mode::Lorawan, hw).expect("plan");
        let systemd = FakeSystemd::new(Some(0));
        match execute(&p, &systemd) {
            Applied::RebootRequired { refused } => assert!(refused.contains("masked")),
            other => panic!("{other:?}"),
        }
        // The stop refused: the restart is not attempted on a half-stopped LoRaWAN set.
        let p = plan(Mode::Lorawan, Mode::P2p, hw).expect("plan");
        let systemd = FakeSystemd::new(Some(0));
        assert!(matches!(
            execute(&p, &systemd),
            Applied::RebootRequired { .. }
        ));
        assert_eq!(systemd.calls().len(), 1, "no restart after a refused stop");
    }

    trait MapToStrings {
        fn map_to_strings(&self) -> Vec<String>;
    }

    impl MapToStrings for Vec<&'static str> {
        fn map_to_strings(&self) -> Vec<String> {
            self.iter().map(|s| s.to_string()).collect()
        }
    }
}
