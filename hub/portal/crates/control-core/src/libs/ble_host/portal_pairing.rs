//! `PortalPairing` — FB1A's write, as a `pair/invite` publish.
//!
//! `plan/control/ble/peripheral.md` §6.5, GitLab #61. The operator is standing at the Hub with a
//! phone: the phone reaches the Hub over BLE and does not reach the portal over HTTP, which is why
//! the in-app code entry had to be hidden in the wizard. This hands the code to the portal running
//! on this same machine and asks it to do what an admin clicking "Pair" would.
//!
//! **What this is not.** It is not a check of the code. The pairing protocol uses the code as the
//! PBKDF2 password the *response* is encrypted under, so the device has nothing to compare it
//! against — a wrong code fails at the portal, when the decryption does. The success this seam
//! reports is "the portal has been asked", and the message says so rather than claiming a pairing
//! that has not happened yet.

use control_ble::host::{Origin, PairingOutcome, PortalPairing};

use crate::libs::mqtt::messages::MqttMessage;

pub struct MqttPortalPairing {
    tx: crossbeam::channel::Sender<MqttMessage>,
}

impl MqttPortalPairing {
    pub fn new(tx: crossbeam::channel::Sender<MqttMessage>) -> MqttPortalPairing {
        MqttPortalPairing { tx }
    }
}

fn origin_str(origin: Origin) -> &'static str {
    match origin {
        Origin::Cli => "cli",
        Origin::Ble => "ble",
        Origin::SignedMqtt => "signed-mqtt",
    }
}

impl PortalPairing for MqttPortalPairing {
    fn submit_code(&self, origin: Origin, code: &str) -> PairingOutcome {
        let msg = MqttMessage::PublishPairingInvite {
            code: code.to_string(),
            origin: origin_str(origin).to_string(),
        };
        // A full or closed channel is the MQTT thread being down, which means the portal will not
        // hear this. Saying so is the whole value of the write-then-read convention: the operator
        // is told to open the portal themselves rather than left watching a wizard that finished.
        match self.tx.try_send(msg) {
            Ok(()) => PairingOutcome::ok(
                "the code was handed to this Hub's portal; pairing completes there",
            ),
            Err(e) => {
                eprintln!("[ble_host] dropped a pairing invite: {e}");
                PairingOutcome::refused(
                    "this Hub's MQTT link is down, so its portal could not be asked to pair",
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_submitted_code_becomes_one_invite_carrying_its_origin() {
        let (tx, rx) = crossbeam::channel::unbounded();
        let seam = MqttPortalPairing::new(tx);
        assert!(seam.submit_code(Origin::Ble, "23456X").success);

        match rx.try_recv().expect("one message") {
            MqttMessage::PublishPairingInvite { code, origin } => {
                assert_eq!(code, "23456X");
                assert_eq!(origin, "ble", "the audit row records which surface asked");
            }
            other => panic!("unexpected message: {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "exactly one");
    }

    #[test]
    fn a_dead_mqtt_thread_is_a_refusal_with_a_reason_not_a_silent_success() {
        // The operator has to learn that the portal was never asked, or the wizard ends claiming
        // a pairing that will never happen.
        let (tx, rx) = crossbeam::channel::bounded(0);
        drop(rx);
        let outcome = MqttPortalPairing::new(tx).submit_code(Origin::Ble, "23456X");
        assert!(!outcome.success);
        assert!(outcome.message.contains("MQTT"), "{}", outcome.message);
    }
}
