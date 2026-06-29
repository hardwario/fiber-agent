//! Shared "add a LoRaWAN sticker" path.
//!
//! This is the single canonical full add: ChirpStack provision + save-and-feed
//! provisioning-epoch bump + sensor-config YAML + optimistic shared-state stub.
//! It is called both by the MQTT command handler
//! (`MqttCommand::AddLoRaWANSticker`) and by the BLE FB0D Sticker-Add
//! characteristic, so the two transports stay byte-for-byte identical.
//!
//! Extracted verbatim from the former inline match arm in `mqtt/monitor.rs` —
//! callers pass their handles via [`StickerAddDeps`]; everything else (the
//! ChirpStack credentials) is read from disk by the provisioning functions, so
//! it is not a dependency here.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::libs::config::LoRaWANSensorConfig;
use crate::libs::config_applier::ConfigApplier;
use crate::libs::lorawan::state::LoRaWANAlarmState;
use crate::libs::lorawan::{LoRaWANSensorState, SharedLoRaWANSensorConfigs, SharedLoRaWANState};
use crate::libs::mqtt::messages::ActivationMode;
use crate::libs::storage::StorageHandle;

/// Handles the full add path needs. All optional — a missing handle degrades
/// the same way the original handler did (e.g. no config applier → hard error).
#[derive(Clone, Default)]
pub struct StickerAddDeps {
    pub config_applier: Option<Arc<ConfigApplier>>,
    pub storage: Option<StorageHandle>,
    pub lorawan_configs: Option<SharedLoRaWANSensorConfigs>,
    pub lorawan_state: Option<SharedLoRaWANState>,
}

/// Provision a sticker in ChirpStack and persist its sensor config.
///
/// Returns `Ok(())` once the sensor config is saved. ChirpStack provisioning is
/// best-effort (logged but not fatal — the device may be down or the sticker
/// may already exist); the hard failure is an absent config applier or a failed
/// config save. Mirrors the original `monitor.rs` behaviour exactly.
pub fn add_lorawan_sticker(
    deps: &StickerAddDeps,
    dev_eui: String,
    name: String,
    serial_number: String,
    activation: ActivationMode,
) -> Result<(), String> {
    let mode_label = match &activation {
        ActivationMode::Otaa { .. } => "OTAA",
        ActivationMode::Abp { .. } => "ABP",
    };
    eprintln!("[sticker_add] Provisioning sticker {} in ChirpStack ({})...", dev_eui, mode_label);
    let provision_result = match &activation {
        ActivationMode::Otaa { app_key, join_eui } => {
            crate::libs::lorawan::provisioning::provision_sticker_otaa(
                &dev_eui, &name, &serial_number, app_key, join_eui,
            )
        }
        ActivationMode::Abp { devaddr, nwkskey, appskey } => {
            crate::libs::lorawan::provisioning::provision_sticker(
                &dev_eui, &name, &serial_number, devaddr, nwkskey, appskey,
            )
        }
    };
    match provision_result {
        Ok(()) => {
            eprintln!("[sticker_add] ✓ Sticker {} provisioned in ChirpStack", dev_eui);

            // Save-and-feed: bump the provisioning epoch only when this
            // dev_eui was previously absent OR its most recent event was
            // a sticker_removed marker. That way re-provisioning an
            // already-active sticker is idempotent (no spurious epoch
            // change), while a remove → re-add cycle creates a new
            // epoch so the downstream pipeline can tell the new
            // sticker apart from the old one.
            if let Some(storage) = deps.storage.as_ref() {
                match storage.dev_eui_last_event_was_removal_or_absent(dev_eui.clone()) {
                    Ok(true) => match storage.bump_provisioning_epoch(dev_eui.clone()) {
                        Ok(new_epoch) => eprintln!(
                            "[sticker_add] sticker {} provisioning epoch bumped to {}",
                            dev_eui, new_epoch
                        ),
                        Err(e) => eprintln!(
                            "[sticker_add] bump_provisioning_epoch({}) failed: {}",
                            dev_eui, e
                        ),
                    },
                    Ok(false) => {
                        // Re-provision of an already-active sticker; no bump.
                    }
                    Err(e) => eprintln!(
                        "[sticker_add] dev_eui_last_event lookup for {} failed: {}",
                        dev_eui, e
                    ),
                }
            }
        }
        Err(e) => {
            // Log but continue - ChirpStack may be down or device may already exist
            eprintln!("[sticker_add] ⚠ ChirpStack provisioning for {}: {}", dev_eui, e);
        }
    }

    // Step 2: Save sensor config to YAML (always, even if ChirpStack failed)
    if let Some(applier) = deps.config_applier.as_ref() {
        let result = applier.apply_lorawan_sensor_config(
            dev_eui.clone(),
            Some(name.clone()),
            Some(serial_number.clone()),
            None, // location: not set at provisioning
        );
        if result.success {
            if let Some(cfgs) = deps.lorawan_configs.as_ref() {
                if let Ok(mut v) = cfgs.write() {
                    if !v.iter().any(|c| c.dev_eui == dev_eui) {
                        v.push(LoRaWANSensorConfig {
                            dev_eui: dev_eui.clone(),
                            name: Some(name.clone()),
                            serial_number: Some(serial_number.clone()),
                            location: None,
                            enabled: true,
                            field_thresholds: Vec::new(),
                        });
                    }
                }
            }
            // Insert a stub in shared state so the next periodic publish
            // includes the sticker even before its first uplink arrives.
            // Without this, the backend's sync would consider the
            // optimistic entry stale and drop it.
            if let Some(state) = deps.lorawan_state.as_ref() {
                if let Ok(mut s) = state.write() {
                    s.sensors.entry(dev_eui.clone()).or_insert_with(|| LoRaWANSensorState {
                        dev_eui: dev_eui.clone(),
                        name: name.clone(),
                        serial_number: Some(serial_number.clone()),
                        location: None,
                        fields: HashMap::new(),
                        field_alarm_states: HashMap::new(),
                        field_thresholds: Vec::new(),
                        counters: HashMap::new(),
                        recent_events: VecDeque::new(),
                        rssi: None,
                        snr: None,
                        last_seen: None,
                        alarm_state: LoRaWANAlarmState::Disconnected,
                    });
                }
            }
            eprintln!("[sticker_add] ✓ LoRaWAN sticker {} config saved", dev_eui);
            Ok(())
        } else {
            Err(result.error_message.unwrap_or_else(|| "Unknown error".to_string()))
        }
    } else {
        Err("Config applier not initialized".to_string())
    }
}
