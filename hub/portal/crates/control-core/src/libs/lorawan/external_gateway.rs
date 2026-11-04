//! Shared "add / remove an external LoRaWAN gateway" path — the gateway twin of
//! [`super::node_add`] and [`super::node_remove`].
//!
//! Extracted verbatim from the former inline `AddExternalGateway` / `RemoveExternalGateway` arms
//! in `mqtt/monitor.rs`, which stay their only callers. They moved here for the same reason the
//! node arms did: so the v2 radio gate sits in a function a test can call, in front of the
//! legacy client's `provisioning::{provision,deprovision}_external_gateway` (gRPC-web straight at
//! ChirpStack), rather than inside a match arm only a running MQTT monitor reaches.
//!
//! On a Hub whose rendered `radio.mode` names a transport, `proximosctl control.radio
//! gateway-add` / `gateway-remove` own the gateways (`plan/control/radio/lorawan.md` §7), and
//! both functions refuse before anything is written (§9, amended 2026-10-09).

use crate::libs::config_applier::ConfigApplier;

/// The v2 radio gate for the two gateway commands: `Err` with the operator-facing refusal — the
/// MQTT command's error — which names what to run instead.
fn refuse_on_v2_radio(instead: &str) -> Result<(), String> {
    match crate::libs::radio_gate::legacy_lorawan_disabled() {
        Some(why) => Err(format!("{why}. {instead}")),
        None => Ok(()),
    }
}

/// Register an external gateway in ChirpStack and persist it to `lorawan.gateways`.
///
/// `gateway_eui` is already validated and normalized (`build_command_from_challenge`, the only
/// construction site of the command). ChirpStack registration is best-effort (logged but not
/// fatal — ChirpStack may be down; local config is the source of truth and is applied
/// regardless); the hard failure is an absent config applier or a failed config save.
pub fn add_external_gateway(
    config_applier: Option<&ConfigApplier>,
    gateway_eui: String,
    name: String,
) -> Result<(), String> {
    refuse_on_v2_radio("Register this gateway with `proximosctl control.radio gateway-add`")?;

    eprintln!(
        "[external_gateway] Registering external gateway {} in ChirpStack...",
        gateway_eui
    );

    // Step 1: best-effort ChirpStack registration — ChirpStack may be
    // down; local config is the source of truth and is applied regardless.
    match crate::libs::lorawan::provisioning::provision_external_gateway(&gateway_eui, &name) {
        Ok(()) => eprintln!(
            "[external_gateway] ✓ Gateway {} registered in ChirpStack",
            gateway_eui
        ),
        Err(e) => eprintln!(
            "[external_gateway] ⚠ ChirpStack gateway provisioning for {}: {}",
            gateway_eui, e
        ),
    }

    // Step 2: persist to YAML (always, even if ChirpStack failed)
    let Some(applier) = config_applier else {
        return Err("Config applier not initialized".to_string());
    };
    let result = applier.apply_external_gateway(gateway_eui.clone(), Some(name));
    if result.success {
        eprintln!(
            "[external_gateway] ✓ External gateway {} config saved",
            gateway_eui
        );
        Ok(())
    } else {
        Err(result
            .error_message
            .unwrap_or_else(|| "Unknown error".to_string()))
    }
}

/// Remove an external gateway from `lorawan.gateways` and deregister it from ChirpStack.
///
/// The config is removed first; deregistration is best-effort (logged but not fatal — local
/// config is the source of truth). The hard failure is an absent config applier or a failed
/// config save.
pub fn remove_external_gateway(
    config_applier: Option<&ConfigApplier>,
    gateway_eui: String,
) -> Result<(), String> {
    refuse_on_v2_radio("Remove this gateway with `proximosctl control.radio gateway-remove`")?;

    eprintln!(
        "[external_gateway] Removing external gateway {} ...",
        gateway_eui
    );
    let Some(applier) = config_applier else {
        return Err("Config applier not initialized".to_string());
    };
    let result = applier.remove_external_gateway(gateway_eui.clone());
    if !result.success {
        return Err(result
            .error_message
            .unwrap_or_else(|| "Unknown error".to_string()));
    }
    // Best-effort: deregister from ChirpStack so it disappears from
    // the network server too. Failure is logged but not fatal —
    // local config is the source of truth.
    match crate::libs::lorawan::provisioning::deprovision_external_gateway(&gateway_eui) {
        Ok(()) => eprintln!(
            "[external_gateway] ✓ Gateway {} removed from ChirpStack",
            gateway_eui
        ),
        Err(e) => eprintln!(
            "[external_gateway] ⚠ ChirpStack gateway deprovision for {}: {}",
            gateway_eui, e
        ),
    }
    eprintln!(
        "[external_gateway] ✓ External gateway {} removed",
        gateway_eui
    );
    Ok(())
}
