//! `RadioModeControl` — BLE FB1B's read and write, over the agent's own radio-mode path.
//!
//! `plan/control/radio/node_registry.md` §12: the Manager app offers the modes this Hub's
//! hardware runs, with the default preselected, and writes the operator's choice before
//! setup-complete. The write is **the same apply path as the signed `radio-set-mode`**
//! ([`radio_mode::apply`]): the hardware refusal, the `fiber.config.yaml` write with its
//! `SET_RADIO_MODE` audit row (`origin: ble`), then the service restart. This module only
//! translates between `control-ble`'s wire types and control-radio's.
//!
//! On a cluster Client the write is refused: a Client runs the Server's mode, which the
//! Server's portal sets with the signed verb (decision #47). The read is unchanged.

use std::sync::Arc;

use control_ble::host::{Origin, RadioMode, RadioModeControl, RadioModeOutcome, RadioModeStatus};
use control_radio::Mode;

use crate::libs::cluster_state::ClusterDir;
use crate::libs::config_applier::ConfigApplier;
use crate::libs::radio_mode::{self, Machine, ThisHub};

pub struct ApplierRadioMode {
    applier: Arc<ConfigApplier>,
    machine: Arc<dyn Machine>,
    cluster: ClusterDir,
}

impl ApplierRadioMode {
    /// The binding the agent runs: this Hub's rendered mode, card and systemd.
    pub fn new(applier: Arc<ConfigApplier>) -> ApplierRadioMode {
        ApplierRadioMode::with_machine(applier, Arc::new(ThisHub))
    }

    /// The same binding on another machine — a test's, which restarts nothing.
    pub fn with_machine(
        applier: Arc<ConfigApplier>,
        machine: Arc<dyn Machine>,
    ) -> ApplierRadioMode {
        ApplierRadioMode {
            applier,
            machine,
            cluster: ClusterDir::at_default(),
        }
    }

    /// The same binding with the cluster directory elsewhere — a test's.
    pub fn with_cluster(mut self, cluster: ClusterDir) -> ApplierRadioMode {
        self.cluster = cluster;
        self
    }

    /// `Some(refusal)` on a cluster Client, whose mode is the Server's (decision #47). An
    /// unreadable marker refuses too: a Hub that cannot tell must not act as a Server.
    fn refuse_on_client(&self) -> Option<String> {
        match self.cluster.is_client() {
            Ok(false) => None,
            Ok(true) => Some(format!(
                "radio mode is set by the Server {}",
                self.cluster
                    .server_host()
                    .unwrap_or_else(|| "this Hub joined".to_string())
            )),
            Err(e) => Some(format!("cannot read the cluster role marker: {e}")),
        }
    }
}

/// `control-ble`'s mode and control-radio's are deliberately separate types (neither crate
/// depends on the other); this is the one place they meet.
fn wire(mode: Mode) -> RadioMode {
    match mode {
        Mode::Off => RadioMode::Off,
        Mode::Lorawan => RadioMode::Lorawan,
        Mode::P2p => RadioMode::P2p,
        Mode::Dual => RadioMode::Dual,
    }
}

fn origin_label(origin: Origin) -> &'static str {
    match origin {
        Origin::Cli => "cli",
        Origin::Ble => "ble",
        Origin::SignedMqtt => "signed-mqtt",
    }
}

impl RadioModeControl for ApplierRadioMode {
    fn read(&self) -> Result<RadioModeStatus, String> {
        let s = radio_mode::status(&self.applier, self.machine.as_ref())?;
        Ok(RadioModeStatus {
            mode: wire(s.mode),
            chosen: s.chosen,
            lorawan_card: s.lorawan_card,
            allowed: s.allowed.into_iter().map(wire).collect(),
            default: wire(s.default),
        })
    }

    fn set(&self, origin: Origin, mode: RadioMode) -> RadioModeOutcome {
        if let Some(why) = self.refuse_on_client() {
            eprintln!("[radio_mode] via {}: refused: {why}", origin_label(origin));
            return RadioModeOutcome::refused(why);
        }
        match radio_mode::apply(
            mode.as_str(),
            &self.applier,
            origin_label(origin),
            self.machine.as_ref(),
        ) {
            Ok(changed) => {
                eprintln!(
                    "[radio_mode] via {}: {}",
                    origin_label(origin),
                    changed.message()
                );
                RadioModeOutcome::ok(changed.message())
            }
            Err(why) => RadioModeOutcome::refused(why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use control_radio::setmode::Plan;
    use control_radio::Hardware;

    struct Bench {
        rendered: Mode,
        hardware: Hardware,
        restarts: std::sync::Mutex<Vec<Plan>>,
    }

    impl Machine for Bench {
        fn rendered_mode(&self) -> Result<Mode, String> {
            Ok(self.rendered)
        }
        fn hardware(&self) -> Hardware {
            self.hardware
        }
        fn restart(&self, plan: Plan) -> Result<(), String> {
            self.restarts.lock().expect("lock").push(plan);
            Ok(())
        }
    }

    fn setup(
        hardware: Hardware,
        rendered: Mode,
    ) -> (tempfile::TempDir, ApplierRadioMode, Arc<Bench>) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("fiber.config.yaml"),
            "system:\n  device_label: bench\n",
        )
        .expect("config");
        let applier = Arc::new(ConfigApplier::new(dir.path()).expect("applier"));
        let bench = Arc::new(Bench {
            rendered,
            hardware,
            restarts: std::sync::Mutex::new(Vec::new()),
        });
        let seam = ApplierRadioMode::with_machine(applier, bench.clone())
            .with_cluster(ClusterDir::new(dir.path().join("cluster")));
        (dir, seam, bench)
    }

    fn file(dir: &tempfile::TempDir) -> serde_yaml::Value {
        serde_yaml::from_str(
            &std::fs::read_to_string(dir.path().join("fiber.config.yaml")).expect("read"),
        )
        .expect("yaml")
    }

    /// The provisioning wizard's first read on a Hub with the card, nothing chosen yet.
    #[test]
    fn a_fresh_hub_with_the_card_offers_every_mode_with_lorawan_preselected() {
        let (_dir, seam, _) = setup(Hardware::NorthbridgeAndConcentrator, Mode::Lorawan);
        assert_eq!(
            seam.read().expect("readable"),
            RadioModeStatus {
                mode: RadioMode::Lorawan,
                chosen: false,
                lorawan_card: true,
                allowed: vec![
                    RadioMode::Off,
                    RadioMode::P2p,
                    RadioMode::Lorawan,
                    RadioMode::Dual
                ],
                default: RadioMode::Lorawan,
            }
        );
    }

    /// A write goes the signed verb's way: written with the BLE origin, restart planned, and the
    /// read after it reports the choice.
    #[test]
    fn a_write_changes_the_mode_through_the_shared_apply_path() {
        let (dir, seam, bench) = setup(Hardware::NorthbridgeAndConcentrator, Mode::Lorawan);
        let outcome = seam.set(Origin::Ble, RadioMode::P2p);
        assert!(outcome.success, "{}", outcome.message);
        assert!(outcome.message.contains("p2p"), "{}", outcome.message);
        assert_eq!(file(&dir)["radio"]["mode"], "p2p");
        let restarts = bench.restarts.lock().expect("lock").clone();
        assert_eq!(restarts.len(), 1, "one restart, planned lorawan → p2p");
        assert_eq!(
            (restarts[0].from, restarts[0].to),
            (Mode::Lorawan, Mode::P2p)
        );
        let after = seam.read().expect("readable");
        assert_eq!((after.mode, after.chosen), (RadioMode::P2p, true));
    }

    /// A cluster Client refuses the write with the Server's name, and changes nothing; the
    /// read still answers (decision #47).
    #[test]
    fn a_client_refuses_the_write_and_names_the_server() {
        let (dir, seam, bench) = setup(Hardware::NorthbridgeAndConcentrator, Mode::P2p);
        let cluster = dir.path().join("cluster");
        std::fs::create_dir(&cluster).expect("dir");
        std::fs::write(cluster.join("server_host"), "proximos-4e970f4a.local\n").expect("host");
        std::fs::write(cluster.join("role"), "client\n").expect("role");
        let outcome = seam.set(Origin::Ble, RadioMode::Lorawan);
        assert!(!outcome.success);
        assert_eq!(
            outcome.message,
            "radio mode is set by the Server proximos-4e970f4a.local"
        );
        assert!(file(&dir).get("radio").is_none(), "nothing written");
        assert!(bench.restarts.lock().expect("lock").is_empty());
        assert_eq!(seam.read().expect("readable").mode, RadioMode::P2p);
    }

    /// Without the card, `lorawan` is refused with the Hub's reason, and nothing is written or
    /// restarted — the refusal the signed verb gives.
    #[test]
    fn lorawan_without_the_card_is_refused_before_anything_changes() {
        let (dir, seam, bench) = setup(Hardware::NorthbridgeOnly, Mode::P2p);
        let outcome = seam.set(Origin::Ble, RadioMode::Lorawan);
        assert!(!outcome.success);
        assert!(outcome.message.contains("RAK5146"), "{}", outcome.message);
        assert!(file(&dir).get("radio").is_none(), "nothing written");
        assert!(bench.restarts.lock().expect("lock").is_empty());
        assert!(!seam.read().expect("readable").chosen);
    }

    /// Confirming the preselected default is a choice too: recorded, no restart.
    #[test]
    fn confirming_the_default_records_the_choice_without_a_restart() {
        let (dir, seam, bench) = setup(Hardware::NorthbridgeOnly, Mode::P2p);
        let outcome = seam.set(Origin::Ble, RadioMode::P2p);
        assert!(outcome.success, "{}", outcome.message);
        assert_eq!(file(&dir)["radio"]["mode"], "p2p");
        assert!(bench.restarts.lock().expect("lock").is_empty());
        assert!(seam.read().expect("readable").chosen);
    }

    /// Every success message arrives whole beside the largest state at the MTU phones
    /// negotiate (peripheral.md §6.6): only a long refusal is ever shortened.
    #[test]
    fn every_success_message_fits_fb1b_whole_at_mtu_185() {
        use control_ble::peripheral::gatt::{radio_mode::document, WriteResult};
        let largest = RadioModeStatus {
            mode: RadioMode::Lorawan,
            chosen: true,
            lorawan_card: true,
            allowed: vec![
                RadioMode::Off,
                RadioMode::P2p,
                RadioMode::Lorawan,
                RadioMode::Dual,
            ],
            default: RadioMode::Lorawan,
        };
        let mut messages = Vec::new();
        for from in Mode::ALL {
            for to in Mode::ALL {
                messages.push(radio_mode::Changed::Restarting { from, to }.message());
            }
            messages.push(radio_mode::Changed::Recorded(from).message());
            messages.push(radio_mode::Changed::Unchanged(from).message());
        }
        for message in messages {
            let bytes =
                document(&largest, Some(&WriteResult::ok(message.clone())), 185).expect("encodes");
            assert!(bytes.len() <= 184, "{message}: {} bytes", bytes.len());
            let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
            assert_eq!(v["message"], message.as_str());
        }
    }

    /// FB1B and the signed `radio-set-mode` leave the same file behind: one path, not two that
    /// agree.
    #[test]
    fn the_ble_write_and_the_signed_verb_are_the_same_write() {
        let (ble_dir, seam, _) = setup(Hardware::NorthbridgeAndConcentrator, Mode::P2p);
        assert!(seam.set(Origin::Ble, RadioMode::Dual).success);

        let (signed_dir, _, bench) = setup(Hardware::NorthbridgeAndConcentrator, Mode::P2p);
        let applier = ConfigApplier::new(signed_dir.path()).expect("applier");
        radio_mode::apply("dual", &applier, "signed-mqtt", bench.as_ref()).expect("changed");

        assert_eq!(file(&ble_dir), file(&signed_dir));
    }
}
