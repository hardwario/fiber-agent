//! GATT application assembly for the Fiber BLE service (FB00).
//!
//! Builds all eight characteristics and wires up the per-module helpers.
//! Callers receive an `Application` ready to register with BlueZ and an
//! `mpsc::Sender<BleEvent>` through which they observe auth/wifi transitions.

use std::sync::Arc;
use std::time::Duration;

use bluer::gatt::local::{
    Application, Characteristic, CharacteristicNotify, CharacteristicNotifyMethod,
    CharacteristicRead, CharacteristicWrite, CharacteristicWriteMethod, ReqError, Service,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

// --- UUID constants -----------------------------------------------------------

pub const HUB_SERVICE_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB00_0000_1000_8000_00805F9B34FB);

const AUTH_CHAR_UUID: uuid::Uuid = uuid::Uuid::from_u128(0x0000FB01_0000_1000_8000_00805F9B34FB);
const WIFI_SCAN_CHAR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB02_0000_1000_8000_00805F9B34FB);
const WIFI_CONNECT_CHAR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB03_0000_1000_8000_00805F9B34FB);
const WIFI_STATUS_CHAR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB04_0000_1000_8000_00805F9B34FB);
const TERMINAL_TX_CHAR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB05_0000_1000_8000_00805F9B34FB);
const TERMINAL_RX_CHAR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB06_0000_1000_8000_00805F9B34FB);
const DEVICE_INFO_CHAR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB07_0000_1000_8000_00805F9B34FB);
const WIFI_DISCONNECT_CHAR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB08_0000_1000_8000_00805F9B34FB);
const DEVICE_LABEL_CHAR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB0A_0000_1000_8000_00805F9B34FB);
const TIME_SET_CHAR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB0B_0000_1000_8000_00805F9B34FB);
const NODE_ADD_CHAR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB0D_0000_1000_8000_00805F9B34FB);
const BEACON_ADD_CHAR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB0E_0000_1000_8000_00805F9B34FB);
const LAN_CONFIG_CHAR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB09_0000_1000_8000_00805F9B34FB);
const LAN_STATUS_CHAR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB0C_0000_1000_8000_00805F9B34FB);

// --- Application builder -----------------------------------------------------

/// Build and return the FB00 GATT application.
///
/// * `state`           – shared GATT-server state (auth flag, notifier, shell).
/// * `event_tx`        – caller-provided channel for observing transitions.
/// * `enable_terminal` – when `false`, Terminal TX/RX characteristics are omitted.
pub async fn create_gatt_app(
    state: super::state::SharedState,
    event_tx: mpsc::Sender<super::BleEvent>,
    enable_terminal: bool,
) -> bluer::Result<Application> {
    // --- Auth characteristic (FB01) ------------------------------------------
    let auth_char = Characteristic {
        uuid: AUTH_CHAR_UUID.into(),
        write: Some(CharacteristicWrite {
            write: true,
            write_without_response: true,
            method: CharacteristicWriteMethod::Fun(Box::new({
                let state = state.clone();
                let event_tx = event_tx.clone();
                move |new_value, peer_req| {
                    let state = state.clone();
                    let event_tx = event_tx.clone();
                    Box::pin(async move {
                        let token_attempt = String::from_utf8_lossy(&new_value).trim().to_string();
                        let mut state_guard = state.lock().await;

                        // Phone is talking to us → reset the idle timer
                        // before doing anything else. We bump on every op
                        // regardless of auth outcome (matches spec: "no
                        // FB01–FB0x reads/writes for 5 minutes → exit").
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);

                        if crate::libs::ble::gatt::auth::verify_token(
                            &token_attempt,
                            &state_guard.provisioning_session,
                        ) {
                            state_guard.authenticated_peer = Some(peer_req.device_address);
                            let _ = event_tx.try_send(super::BleEvent::AuthSuccess);
                            Ok(())
                        } else {
                            let _ = event_tx.try_send(super::BleEvent::AuthFailed);
                            Err(ReqError::NotAuthorized)
                        }
                    })
                }
            })),
            ..Default::default()
        }),
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new({
                let state = state.clone();
                move |peer_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        let is_auth = state_guard.is_authenticated_for(peer_req.device_address);
                        let response = crate::libs::ble::gatt::auth::auth_response(is_auth);
                        Ok(serde_json::to_vec(&response).unwrap_or_default())
                    })
                }
            }),
            ..Default::default()
        }),
        ..Default::default()
    };

    // --- WiFi Scan characteristic (FB02) -------------------------------------
    let wifi_scan_char = Characteristic {
        uuid: WIFI_SCAN_CHAR_UUID.into(),
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new({
                let state = state.clone();
                move |peer_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.is_authenticated_for(peer_req.device_address) {
                            return Err(ReqError::NotAuthorized);
                        }
                        drop(state_guard);

                        eprintln!("[WiFi] Scanning for networks...");
                        let networks = crate::libs::ble::gatt::wifi::scan_wifi();
                        eprintln!("[WiFi] Found {} networks", networks.len());
                        Ok(serde_json::to_vec(&networks).unwrap_or_default())
                    })
                }
            }),
            ..Default::default()
        }),
        ..Default::default()
    };

    // --- WiFi Connect characteristic (FB03) ----------------------------------
    let wifi_connect_char = Characteristic {
        uuid: WIFI_CONNECT_CHAR_UUID.into(),
        write: Some(CharacteristicWrite {
            write: true,
            method: CharacteristicWriteMethod::Fun(Box::new({
                let state = state.clone();
                let event_tx = event_tx.clone();
                move |new_value, peer_req| {
                    let state = state.clone();
                    let event_tx = event_tx.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.is_authenticated_for(peer_req.device_address) {
                            return Err(ReqError::NotAuthorized);
                        }
                        drop(state_guard);

                        let request: crate::libs::ble::gatt::wifi::WiFiConnectRequest =
                            serde_json::from_slice(&new_value)
                                .map_err(|_| ReqError::InvalidValueLength)?;

                        let _ = event_tx.try_send(super::BleEvent::WifiConnecting {
                            ssid: request.ssid.clone(),
                        });
                        let result = crate::libs::ble::gatt::wifi::connect_wifi(
                            &request.ssid,
                            &request.password,
                        );

                        if result.connected {
                            let _ = event_tx.try_send(super::BleEvent::WifiConnected {
                                ssid: result.ssid.clone(),
                                ip: result.ip_address.clone(),
                            });
                            Ok(())
                        } else {
                            let _ = event_tx.try_send(super::BleEvent::WifiFailed {
                                error: result.error.clone(),
                            });
                            Err(ReqError::Failed)
                        }
                    })
                }
            })),
            ..Default::default()
        }),
        ..Default::default()
    };

    // --- WiFi Disconnect characteristic (FB08) --------------------------------
    let wifi_disconnect_char = Characteristic {
        uuid: WIFI_DISCONNECT_CHAR_UUID.into(),
        write: Some(CharacteristicWrite {
            write: true,
            method: CharacteristicWriteMethod::Fun(Box::new({
                let state = state.clone();
                move |_new_value, peer_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.is_authenticated_for(peer_req.device_address) {
                            return Err(ReqError::NotAuthorized);
                        }
                        drop(state_guard);

                        let result = crate::libs::ble::gatt::wifi::disconnect_wifi();

                        if !result.connected {
                            Ok(())
                        } else {
                            Err(ReqError::Failed)
                        }
                    })
                }
            })),
            ..Default::default()
        }),
        ..Default::default()
    };

    // --- WiFi Status characteristic (FB04) ------------------------------------
    let wifi_status_char = Characteristic {
        uuid: WIFI_STATUS_CHAR_UUID.into(),
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new({
                let state = state.clone();
                move |peer_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.is_authenticated_for(peer_req.device_address) {
                            return Err(ReqError::NotAuthorized);
                        }
                        drop(state_guard);

                        let status = crate::libs::ble::gatt::wifi::get_wifi_status();
                        Ok(serde_json::to_vec(&status).unwrap_or_default())
                    })
                }
            }),
            ..Default::default()
        }),
        notify: Some(CharacteristicNotify {
            notify: true,
            method: CharacteristicNotifyMethod::Fun(Box::new({
                move |notifier| {
                    Box::pin(async move {
                        // Keep notifier alive until client unsubscribes.
                        notifier.stopped().await;
                    })
                }
            })),
            ..Default::default()
        }),
        ..Default::default()
    };

    // --- Device Info characteristic (FB07) ------------------------------------
    let device_info_char = Characteristic {
        uuid: DEVICE_INFO_CHAR_UUID.into(),
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new({
                let state = state.clone();
                move |_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        // Counts as activity even though no auth is required —
                        // any FB0x op should keep the session alive.
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        let info = crate::libs::ble::gatt::device_info::build_response(
                            &state_guard.hostname,
                            &state_guard.mac_address,
                        );
                        Ok(serde_json::to_vec(&info).unwrap_or_default())
                    })
                }
            }),
            ..Default::default()
        }),
        ..Default::default()
    };

    // --- Device Label characteristic (FB0A) -----------------------------------
    // Read returns {"label": "<current>"} pulled from fiber.config.yaml.
    // Write accepts {"label": "<new>"} and routes through ConfigApplier
    // (atomic write + audit log entry). Auth-gated. Display picks up the
    // new value on its next frame thanks to hot-reload in display/monitor.
    let device_label_char = Characteristic {
        uuid: DEVICE_LABEL_CHAR_UUID.into(),
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new({
                let state = state.clone();
                move |peer_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.is_authenticated_for(peer_req.device_address) {
                            return Err(ReqError::NotAuthorized);
                        }
                        let hostname_fallback = state_guard.hostname.clone();
                        drop(state_guard);

                        // Pull fresh from disk rather than caching: a write
                        // via FB0A or MQTT could have changed it since
                        // boot, and the file IS the source of truth.
                        let label = crate::libs::config::Config::load_default()
                            .ok()
                            .and_then(|c| c.system.device_label)
                            .unwrap_or(hostname_fallback);
                        let resp = serde_json::json!({ "label": label });
                        Ok(serde_json::to_vec(&resp).unwrap_or_default())
                    })
                }
            }),
            ..Default::default()
        }),
        write: Some(CharacteristicWrite {
            write: true,
            method: CharacteristicWriteMethod::Fun(Box::new({
                let state = state.clone();
                move |new_value, peer_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.is_authenticated_for(peer_req.device_address) {
                            return Err(ReqError::NotAuthorized);
                        }
                        let applier = match state_guard.config_applier.clone() {
                            Some(a) => a,
                            None => {
                                eprintln!("[DeviceLabel] No ConfigApplier wired; rejecting write");
                                return Err(ReqError::Failed);
                            }
                        };
                        drop(state_guard);

                        // Parse {"label": "<...>"}.
                        #[derive(serde::Deserialize)]
                        struct Req {
                            label: String,
                        }
                        let req: Req = match serde_json::from_slice(&new_value) {
                            Ok(r) => r,
                            Err(e) => {
                                eprintln!("[DeviceLabel] Malformed write payload: {}", e);
                                return Err(ReqError::InvalidValueLength);
                            }
                        };

                        let result = applier.apply_device_label_change(req.label.clone());
                        if result.success {
                            eprintln!("[DeviceLabel] Updated to {:?}", req.label);
                            Ok(())
                        } else {
                            eprintln!(
                                "[DeviceLabel] Rejected: {}",
                                result.error_message.as_deref().unwrap_or("unknown")
                            );
                            Err(ReqError::Failed)
                        }
                    })
                }
            })),
            ..Default::default()
        }),
        ..Default::default()
    };

    // --- Time Set characteristic (FB0B) --------------------------------------
    // Write {"epoch": <utc seconds>} to set the device clock (and persist to
    // RTC) — for BLE-only deployments where NTP is unreachable after a power
    // loss. Read returns {epoch, synchronized}. Auth-gated, mirrors FB09.
    let time_set_char = Characteristic {
        uuid: TIME_SET_CHAR_UUID.into(),
        write: Some(CharacteristicWrite {
            write: true,
            method: CharacteristicWriteMethod::Fun(Box::new({
                let state = state.clone();
                move |new_value, peer_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.is_authenticated_for(peer_req.device_address) {
                            return Err(ReqError::NotAuthorized);
                        }
                        drop(state_guard);

                        let req: crate::libs::ble::gatt::time_sync::TimeSetRequest =
                            serde_json::from_slice(&new_value)
                                .map_err(|_| ReqError::InvalidValueLength)?;

                        // `date` + `hwclock` are quick (sub-second), well under
                        // the ATT write-response timeout, but they shell out —
                        // offload off the async worker thread anyway.
                        let epoch = req.epoch;
                        let result = tokio::task::spawn_blocking(move || {
                            crate::libs::ble::gatt::time_sync::set_system_time(epoch)
                        })
                        .await
                        .unwrap_or_else(|_| Err("internal task error".to_string()));

                        // Setting the clock can step it forward by a long way
                        // (a stale-RTC device being corrected from the phone).
                        // `last_activity` was stamped with the *old* clock at the
                        // touch above, so without this the idle reaper would see a
                        // huge `now - last_activity` delta on its next 50ms tick
                        // and tear the session down (clearing the token + stopping
                        // advertising) mid-provisioning. Re-touch with the now-
                        // corrected clock so the session survives.
                        if result.is_ok() {
                            let state_guard = state.lock().await;
                            crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        }

                        result.map_err(|_| ReqError::Failed)
                    })
                }
            })),
            ..Default::default()
        }),
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new({
                let state = state.clone();
                move |peer_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.is_authenticated_for(peer_req.device_address) {
                            return Err(ReqError::NotAuthorized);
                        }
                        drop(state_guard);
                        let status = crate::libs::ble::gatt::time_sync::get_time_status();
                        Ok(serde_json::to_vec(&status).unwrap_or_default())
                    })
                }
            }),
            ..Default::default()
        }),
        ..Default::default()
    };

    // --- Node Add characteristic (FB0D) ------------------------------------
    // Write {"deveui","joineui","appkey","name","serial_number"} to enroll a
    // LoRaWAN node (OTAA) into the local ChirpStack via the shared add path
    // (same as MQTT's AddLoRaWANNode). Read returns the structured result of
    // the most recent write. Auth-gated, mirrors FB09/FB01.
    let node_add_char = Characteristic {
        uuid: NODE_ADD_CHAR_UUID.into(),
        write: Some(CharacteristicWrite {
            write: true,
            method: CharacteristicWriteMethod::Fun(Box::new({
                let state = state.clone();
                move |new_value, peer_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        use crate::libs::ble::gatt::node;

                        // Bound the request before any parsing work — a
                        // malicious peer could otherwise push megabytes
                        // through serde_json before validation rejects it.
                        if new_value.len() > node::MAX_PAYLOAD_BYTES {
                            return Err(ReqError::InvalidValueLength);
                        }

                        let mut state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.is_authenticated_for(peer_req.device_address) {
                            return Err(ReqError::NotAuthorized);
                        }

                        // Single-pending guard: refuse a new write while a
                        // prior enrollment is still running. Without this,
                        // a fast peer could pile up unbounded blocking
                        // workers and the result slot would be clobbered
                        // mid-flight.
                        let slot = state_guard.node_result.clone();
                        if node::read(&slot).pending {
                            return Err(ReqError::Failed);
                        }

                        let req: node::NodeAddRequest = match serde_json::from_slice(&new_value) {
                            Ok(r) => r,
                            Err(_) => {
                                // Record the failure so the FB0D read
                                // does not surface a stale prior result.
                                node::store(
                                    &slot,
                                    node::NodeAddResponse {
                                        pending: false,
                                        success: false,
                                        message: "invalid json".to_string(),
                                        deveui: String::new(),
                                    },
                                );
                                return Err(ReqError::Failed);
                            }
                        };

                        let prepared = match node::prepare(&req) {
                            Ok(p) => p,
                            Err(msg) => {
                                node::store(
                                    &slot,
                                    node::NodeAddResponse {
                                        pending: false,
                                        success: false,
                                        message: msg,
                                        deveui: req.deveui.trim().to_lowercase(),
                                    },
                                );
                                return Err(ReqError::Failed);
                            }
                        };

                        // add_lorawan_node drives ChirpStack gRPC + disk
                        // writes that take seconds — longer than the BLE
                        // write-response ACK. So we ACK the write immediately
                        // and run the enrollment in the background; the
                        // client polls FB0D read (pending=true → final
                        // result). A synchronous wait here returned a GATT
                        // UNLIKELY_ERROR on a real device even though the
                        // add succeeded.
                        let deps = state_guard.node_deps();
                        let dev_eui = prepared.dev_eui.clone();
                        if !node::try_begin(&slot, dev_eui.clone()) {
                            // Lost a race against another concurrent writer
                            // that got the slot first; behave like the
                            // pending-guard above.
                            return Err(ReqError::Failed);
                        }

                        // Drop any prior task handle (it must already be
                        // done — the pending-guard guarantees this) before
                        // we own the new one.
                        if let Some(prev) = state_guard.node_task.take() {
                            prev.abort();
                        }
                        let slot_for_task = slot.clone();
                        let handle = tokio::spawn(async move {
                            let result = tokio::task::spawn_blocking(move || {
                                crate::libs::lorawan::add_lorawan_node(
                                    &deps,
                                    prepared.dev_eui,
                                    prepared.name,
                                    prepared.serial_number,
                                    prepared.activation,
                                )
                            })
                            .await
                            .unwrap_or_else(|join_err| {
                                eprintln!("[gatt::node] enrollment task failed: {join_err}");
                                Err("internal task error".to_string())
                            });

                            let (success, message) = match &result {
                                Ok(()) => (true, "node enrolled".to_string()),
                                Err(e) => (false, e.clone()),
                            };
                            node::store(
                                &slot_for_task,
                                node::NodeAddResponse {
                                    pending: false,
                                    success,
                                    message,
                                    deveui: dev_eui,
                                },
                            );
                        });
                        state_guard.node_task = Some(handle);
                        drop(state_guard);
                        Ok(())
                    })
                }
            })),
            ..Default::default()
        }),
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new({
                let state = state.clone();
                move |peer_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        use crate::libs::ble::gatt::node;
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.is_authenticated_for(peer_req.device_address) {
                            return Err(ReqError::NotAuthorized);
                        }
                        let slot = state_guard.node_result.clone();
                        drop(state_guard);
                        let resp = node::read(&slot);
                        Ok(serde_json::to_vec(&resp).unwrap_or_default())
                    })
                }
            }),
            ..Default::default()
        }),
        ..Default::default()
    };

    // --- EYE Tag Add characteristic (FB0E) ------------------------------------
    // Write {"mac","name"?} to pair a Teltonika EYE (BTSMP1) BLE sensor tag with
    // this FIBER: it is enrolled into eye.tags[] via the shared add_eye_tag path
    // (same as MQTT's AddBeaconTag) and the running scan starts tracking it without
    // a restart. Read returns the structured result of the most recent write.
    // Auth-gated, mirrors FB0D — but enrollment is a synchronous local YAML write
    // (no ChirpStack), so there is no background task / pending poll. Issue #84.
    let beacon_add_char = Characteristic {
        uuid: BEACON_ADD_CHAR_UUID.into(),
        write: Some(CharacteristicWrite {
            write: true,
            method: CharacteristicWriteMethod::Fun(Box::new({
                let state = state.clone();
                move |new_value, peer_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        use crate::libs::ble::gatt::beacon_add;

                        // Bound the request before any parsing work.
                        if new_value.len() > beacon_add::MAX_PAYLOAD_BYTES {
                            return Err(ReqError::InvalidValueLength);
                        }

                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.is_authenticated_for(peer_req.device_address) {
                            return Err(ReqError::NotAuthorized);
                        }
                        let slot = state_guard.beacon_add_result.clone();

                        let req: beacon_add::BeaconAddRequest =
                            match serde_json::from_slice(&new_value) {
                                Ok(r) => r,
                                Err(_) => {
                                    beacon_add::store(
                                        &slot,
                                        beacon_add::BeaconAddResponse {
                                            success: false,
                                            message: "invalid json".to_string(),
                                        },
                                    );
                                    return Err(ReqError::Failed);
                                }
                            };

                        let prepared = match beacon_add::prepare(&req) {
                            Ok(p) => p,
                            Err(msg) => {
                                beacon_add::store(
                                    &slot,
                                    beacon_add::BeaconAddResponse {
                                        success: false,
                                        message: msg,
                                    },
                                );
                                return Err(ReqError::Failed);
                            }
                        };

                        let applier = match state_guard.config_applier.clone() {
                            Some(a) => a,
                            None => {
                                beacon_add::store(
                                    &slot,
                                    beacon_add::BeaconAddResponse {
                                        success: false,
                                        message: "config applier not initialized".to_string(),
                                    },
                                );
                                return Err(ReqError::Failed);
                            }
                        };
                        // Release the GATT-state lock before the (fast, local)
                        // disk write so other characteristics are not blocked.
                        drop(state_guard);

                        // Persist to eye.tags[] (atomic + rollback inside the applier).
                        let result = applier
                            .apply_beacon_tag_config(prepared.mac.clone(), prepared.name.clone());
                        if !result.success {
                            let message = result
                                .error_message
                                .unwrap_or_else(|| "unknown error".to_string());
                            beacon_add::store(
                                &slot,
                                beacon_add::BeaconAddResponse {
                                    success: false,
                                    message,
                                },
                            );
                            return Err(ReqError::Failed);
                        }

                        // Reflect into the running scan's live config + seed the
                        // in-memory state (uppercase MAC key), mirroring the
                        // mqtt::monitor AddBeaconTag arm so the monitor tracks the
                        // tag without a restart.
                        if let Some(cfg) = crate::libs::beacon::state::beacon_config_handle() {
                            if let Ok(mut c) = cfg.write() {
                                c.upsert_tag(&prepared.mac, prepared.name.as_deref());
                            }
                        }
                        if let Some(handle) = crate::libs::beacon::state::beacon_state_handle() {
                            if let Ok(mut s) = handle.write() {
                                let entry = s.entry(&prepared.mac, prepared.name.clone());
                                if let Some(n) = prepared.name.clone() {
                                    entry.name = Some(n);
                                }
                                // See mqtt::monitor's AddBeaconTag arm: clear it
                                // explicitly rather than rely on the scan loop's
                                // next reconcile, since this MAC may be enrolled
                                // while out of range.
                                entry.discovered = false;
                            }
                        }

                        eprintln!(
                            "[gatt::beacon_add] ✓ EYE tag {} enrolled via FB0E",
                            prepared.mac
                        );
                        beacon_add::store(
                            &slot,
                            beacon_add::BeaconAddResponse {
                                success: true,
                                message: String::new(),
                            },
                        );
                        Ok(())
                    })
                }
            })),
            ..Default::default()
        }),
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new({
                let state = state.clone();
                move |peer_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        use crate::libs::ble::gatt::beacon_add;
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.is_authenticated_for(peer_req.device_address) {
                            return Err(ReqError::NotAuthorized);
                        }
                        let slot = state_guard.beacon_add_result.clone();
                        drop(state_guard);
                        let resp = beacon_add::read(&slot);
                        Ok(serde_json::to_vec(&resp).unwrap_or_default())
                    })
                }
            }),
            ..Default::default()
        }),
        ..Default::default()
    };

    // --- LAN Config characteristic (FB09) -------------------------------------
    // Write {"mode":"dhcp"} or {"mode":"static","ipv4":{...}} to configure the
    // wired interface via NetworkManager. Auth-gated, mirrors FB03.
    let lan_config_char = Characteristic {
        uuid: LAN_CONFIG_CHAR_UUID.into(),
        write: Some(CharacteristicWrite {
            write: true,
            method: CharacteristicWriteMethod::Fun(Box::new({
                let state = state.clone();
                let event_tx = event_tx.clone();
                move |new_value, peer_req| {
                    let state = state.clone();
                    let event_tx = event_tx.clone();
                    Box::pin(async move {
                        use crate::libs::ble::gatt::lan;
                        use std::sync::atomic::Ordering as AtomicOrdering;

                        // Bound the request before any parsing work — a
                        // malicious peer could otherwise push megabytes
                        // through serde_json before validation rejects it.
                        if new_value.len() > lan::MAX_PAYLOAD_BYTES {
                            return Err(ReqError::InvalidValueLength);
                        }

                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.is_authenticated_for(peer_req.device_address) {
                            return Err(ReqError::NotAuthorized);
                        }
                        let in_flight = state_guard.lan_apply_in_flight.clone();
                        drop(state_guard);

                        // Single-pending guard: refuse a new write while a
                        // prior `apply_lan_config` is still running. Without
                        // this, a peer can spam FB09 writes and either
                        // saturate the blocking pool or trigger a
                        // NetworkManager modify+up race that leaves the eth
                        // profile half-applied.
                        if in_flight
                            .compare_exchange(
                                false,
                                true,
                                AtomicOrdering::SeqCst,
                                AtomicOrdering::SeqCst,
                            )
                            .is_err()
                        {
                            return Err(ReqError::Failed);
                        }
                        // RAII reset: clear the flag no matter how this
                        // closure exits below.
                        struct InFlightGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
                        impl Drop for InFlightGuard {
                            fn drop(&mut self) {
                                self.0.store(false, std::sync::atomic::Ordering::SeqCst);
                            }
                        }
                        let _guard = InFlightGuard(in_flight);

                        let request: lan::LanConfigRequest = serde_json::from_slice(&new_value)
                            .map_err(|_| ReqError::InvalidValueLength)?;

                        // apply_lan_config drives nmcli synchronously and `up`
                        // can block for seconds — offload it so the async BLE
                        // worker thread is not tied up while NM works.
                        let apply_result =
                            tokio::task::spawn_blocking(move || lan::apply_lan_config(&request))
                                .await
                                .unwrap_or(Err(
                                    crate::libs::ble::gatt::net_error::NetworkErrorCategory::Other,
                                ));

                        match apply_result {
                            Ok(()) => {
                                let status = lan::get_lan_status();
                                let _ = event_tx.try_send(super::BleEvent::LanConfigured {
                                    mode: status.mode.clone(),
                                    ip: status.ip_address.clone(),
                                });
                                Ok(())
                            }
                            Err(category) => {
                                let _ = event_tx.try_send(super::BleEvent::LanFailed {
                                    error: category.as_str().to_string(),
                                });
                                Err(ReqError::Failed)
                            }
                        }
                    })
                }
            })),
            ..Default::default()
        }),
        ..Default::default()
    };

    // --- LAN Status characteristic (FB0C) -------------------------------------
    // Read returns {connected, link, ip_address, mac, mode, error}. Notify is
    // passive (kept alive; client polls via read) — identical to FB04.
    let lan_status_char = Characteristic {
        uuid: LAN_STATUS_CHAR_UUID.into(),
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new({
                let state = state.clone();
                move |peer_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.is_authenticated_for(peer_req.device_address) {
                            return Err(ReqError::NotAuthorized);
                        }
                        drop(state_guard);

                        let status = crate::libs::ble::gatt::lan::get_lan_status();
                        Ok(serde_json::to_vec(&status).unwrap_or_default())
                    })
                }
            }),
            ..Default::default()
        }),
        notify: Some(CharacteristicNotify {
            notify: true,
            method: CharacteristicNotifyMethod::Fun(Box::new({
                move |notifier| {
                    Box::pin(async move {
                        // Keep notifier alive until client unsubscribes.
                        notifier.stopped().await;
                    })
                }
            })),
            ..Default::default()
        }),
        ..Default::default()
    };

    // --- Terminal TX characteristic (FB05) ------------------------------------
    let terminal_tx_char = Characteristic {
        uuid: TERMINAL_TX_CHAR_UUID.into(),
        write: Some(CharacteristicWrite {
            write: true,
            write_without_response: true,
            method: CharacteristicWriteMethod::Fun(Box::new({
                let state = state.clone();
                move |new_value, peer_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let command = String::from_utf8_lossy(&new_value).trim().to_string();

                        // Fetch auth flag + shared handles without holding the lock
                        // across the slow shell-spawn path.
                        let (is_authenticated, notifier_opt, shell_opt) = {
                            let state_guard = state.lock().await;
                            crate::libs::network::touch_shared(&state_guard.provisioning_session);
                            (
                                state_guard.is_authenticated_for(peer_req.device_address),
                                state_guard.terminal_notifier.clone(),
                                state_guard.shell_process.clone(),
                            )
                        };

                        if !is_authenticated {
                            return Err(ReqError::NotAuthorized);
                        }

                        eprintln!("[Terminal] Command: {}", command);

                        // Security / policy filter (replaces inline blocklists).
                        match crate::libs::ble::gatt::terminal::classify_command(&command) {
                            crate::libs::ble::gatt::terminal::CommandPolicy::Reject(msg) => {
                                if let Some(ref notifier) = notifier_opt {
                                    let mut n = notifier.lock().await;
                                    let mut bytes = msg.as_bytes().to_vec();
                                    bytes.push(b'\n');
                                    let _ = n.notify(bytes).await;
                                }
                                return Ok(());
                            }
                            crate::libs::ble::gatt::terminal::CommandPolicy::Allow => {}
                        }

                        // Get or create the persistent shell.
                        eprintln!("[Terminal] Getting/creating shell...");
                        let shell = match shell_opt {
                            Some(s) => {
                                eprintln!("[Terminal] Using existing shell");
                                s
                            }
                            None => {
                                eprintln!("[Terminal] Creating new shell...");
                                let notifier = match notifier_opt {
                                    Some(n) => n,
                                    None => {
                                        eprintln!("[Terminal] No notifier available");
                                        return Err(ReqError::Failed);
                                    }
                                };

                                eprintln!("[Terminal] Calling spawn_persistent_shell...");
                                match crate::libs::ble::gatt::terminal::spawn_persistent_shell(
                                    notifier,
                                )
                                .await
                                {
                                    Ok(shell) => {
                                        eprintln!(
                                            "[Terminal] spawn returned OK, wrapping in Arc..."
                                        );
                                        let shell_arc = Arc::new(Mutex::new(shell));
                                        eprintln!("[Terminal] Storing shell in state...");
                                        {
                                            let mut state_guard = state.lock().await;
                                            state_guard.shell_process = Some(shell_arc.clone());
                                        }
                                        eprintln!("[Terminal] Shell initialized and stored");
                                        shell_arc
                                    }
                                    Err(e) => {
                                        eprintln!("[Terminal] Failed to spawn shell: {}", e);
                                        return Err(ReqError::Failed);
                                    }
                                }
                            }
                        };
                        eprintln!("[Terminal] Shell ready, sending command...");

                        // Forward command to shell via stdin.
                        {
                            let mut shell_guard = shell.lock().await;
                            let cmd = format!("{}\n", command);
                            if let Err(e) = shell_guard.stdin.write_all(cmd.as_bytes()).await {
                                eprintln!("[Terminal] Shell write error: {}", e);
                                return Err(ReqError::Failed);
                            }
                            let _ = shell_guard.stdin.flush().await;
                        }

                        Ok(())
                    })
                }
            })),
            ..Default::default()
        }),
        ..Default::default()
    };

    // --- Terminal RX characteristic (FB06) ------------------------------------
    let terminal_rx_char = Characteristic {
        uuid: TERMINAL_RX_CHAR_UUID.into(),
        notify: Some(CharacteristicNotify {
            notify: true,
            method: CharacteristicNotifyMethod::Fun(Box::new({
                let state = state.clone();
                move |notifier| {
                    let state = state.clone();
                    Box::pin(async move {
                        eprintln!("[Terminal] Client subscribed to RX notifications");
                        {
                            let mut state_guard = state.lock().await;
                            crate::libs::network::touch_shared(&state_guard.provisioning_session);
                            state_guard.terminal_notifier = Some(Arc::new(Mutex::new(notifier)));
                            eprintln!("[Terminal] Notifier stored in state");
                        }
                        // Keep alive until client unsubscribes.
                        loop {
                            tokio::time::sleep(Duration::from_secs(3600)).await;
                        }
                    })
                }
            })),
            ..Default::default()
        }),
        ..Default::default()
    };

    // --- Assemble the service -------------------------------------------------

    let mut chars = vec![
        auth_char,
        wifi_scan_char,
        wifi_connect_char,
        wifi_disconnect_char,
        wifi_status_char,
        device_info_char,
        device_label_char,
        time_set_char,
        node_add_char,
        beacon_add_char,
        lan_config_char,
        lan_status_char,
    ];

    if enable_terminal {
        chars.push(terminal_tx_char);
        chars.push(terminal_rx_char);
    }

    let hub_service = Service {
        uuid: HUB_SERVICE_UUID.into(),
        primary: true,
        characteristics: chars,
        ..Default::default()
    };

    Ok(Application {
        services: vec![hub_service],
        ..Default::default()
    })
}
