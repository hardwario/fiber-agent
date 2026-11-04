//! Shared "remove a LoRaWAN node" path — the mirror of [`super::node_add`].
//!
//! This is the single canonical full remove: sensor-config YAML + drop from the shared state and
//! the shared configs + best-effort ChirpStack deprovision. It is called both by the MQTT command
//! handler (`MqttCommand::RemoveLoRaWANNode`) and by the BLE FB12 Node-Remove characteristic, so
//! the two transports stay byte-for-byte identical.
//!
//! **Why it exists at all.** `plan/control/ble/plan.md` §9.6 states the acceptance criterion for
//! every mutation at the *store layer*: a BLE-originated change and an MQTT-originated change must
//! be indistinguishable in what they leave behind. Two surfaces that agree today because someone
//! wrote them carefully is not the same as two surfaces that cannot disagree. Adding a fourth
//! mutation dialect next to the inline `monitor.rs` arm would have been exactly the former, so the
//! arm moved here and both callers reach the same function.
//!
//! Extracted verbatim from the former inline match arm in `mqtt/monitor.rs` — callers pass their
//! handles via [`NodeAddDeps`], the same bundle the add path takes, because the two paths need the
//! same four handles and a second near-identical struct would only invite them to drift.
//! `storage` is unused here: the add path bumps a provisioning epoch, and a removal deliberately
//! leaves the reading history alone (`remove_lorawan_sensor_config` appends its own
//! `node_removed` marker through the applier).

use crate::libs::lorawan::NodeAddDeps;

/// Remove a node's sensor config and drop it from everything that mirrors it.
///
/// Returns `Ok(())` once the sensor config is saved without it. ChirpStack deprovisioning is
/// best-effort (logged but not fatal — the server may be down, and local config is the source of
/// truth); the hard failure is an absent config applier or a failed config save. Mirrors the
/// original `monitor.rs` behaviour exactly — except on a Hub whose rendered `radio.mode` names a
/// transport, where [`crate::libs::radio_gate`] refuses it before anything is written.
///
/// **It does not decide whether the node exists.** `remove_lorawan_sensor_config` is the authority
/// on that, and callers that need to refuse an unknown device — FB12 does, so an installer's typo
/// is legible — resolve it before calling.
pub fn remove_lorawan_node(deps: &NodeAddDeps, dev_eui: String) -> Result<(), String> {
    // The v2 radio gate (decision #12, `libs::radio_gate`), the mirror of the add path's. The
    // deprovision below is the legacy client's gRPC-web straight at ChirpStack, and this
    // function is reachable without the LoRaWAN monitor: the MQTT `RemoveLoRaWANNode` arm and
    // BLE FB12 both call it. On a Hub where `proximosctl control.radio` owns enrollment it would
    // delete a ChirpStack device out from under that stack's `nodes.json` record and retained
    // `info` document — the same two-stacks-one-radio split the add path's gate prevents.
    //
    // Refused before anything is written: the text is FB12's `message` and the MQTT command's
    // error, so it has to say what to do instead.
    if let Some(why) = crate::libs::radio_gate::legacy_lorawan_disabled() {
        return Err(format!(
            "{why}. Remove this node with `proximosctl control.radio node-remove`"
        ));
    }

    let Some(applier) = deps.config_applier.as_ref() else {
        return Err("Config applier not initialized".to_string());
    };

    let result = applier.remove_lorawan_sensor_config(dev_eui.clone());
    if !result.success {
        return Err(result
            .error_message
            .unwrap_or_else(|| "Unknown error".to_string()));
    }

    // Drop the in-memory sensor entry so the display stops showing it.
    if let Some(state) = deps.lorawan_state.as_ref() {
        if let Ok(mut s) = state.write() {
            s.sensors.remove(&dev_eui);
        }
    }
    // Drop from the shared configs so the LoRa monitor stops evaluating thresholds for it
    // (matches on-disk YAML state).
    if let Some(cfgs) = deps.lorawan_configs.as_ref() {
        if let Ok(mut v) = cfgs.write() {
            v.retain(|c| c.dev_eui != dev_eui);
        }
    }
    // Best-effort: remove from ChirpStack so the device disappears from the network server too.
    // Failure is logged but not fatal — local config is the source of truth.
    match crate::libs::lorawan::provisioning::deprovision_node(&dev_eui) {
        Ok(()) => eprintln!("[node_remove] ✓ Node {} removed from ChirpStack", dev_eui),
        Err(e) => eprintln!(
            "[node_remove] ⚠ ChirpStack deprovision for {}: {}",
            dev_eui, e
        ),
    }
    eprintln!("[node_remove] ✓ LoRaWAN node {} removed", dev_eui);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_config_applier_is_the_hard_error_it_was_in_the_arm() {
        // The one failure the original arm treated as fatal, preserved by name: "failed" alone
        // would send an operator looking at ChirpStack, which is the half that is best-effort.
        let e = remove_lorawan_node(&NodeAddDeps::default(), "0011223344556677".to_string())
            .expect_err("no applier");
        assert!(e.contains("Config applier"), "{e}");
    }
}
