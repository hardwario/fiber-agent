//! GATT application assembly for the Fiber BLE service (FB00).
//!
//! Builds all eight characteristics and wires up the per-module helpers.
//! Callers receive an `Application` ready to register with BlueZ and an
//! `mpsc::Sender<BleEvent>` through which they observe auth/wifi transitions.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use bluer::gatt::local::{
    Application, Characteristic, CharacteristicNotify, CharacteristicNotifyMethod,
    CharacteristicRead, CharacteristicWrite, CharacteristicWriteMethod, ReqError, Service,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

// --- UUID constants -----------------------------------------------------------

pub const FIBER_SERVICE_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB00_0000_1000_8000_00805F9B34FB);

const AUTH_CHAR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB01_0000_1000_8000_00805F9B34FB);
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
const STICKER_ADD_CHAR_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000FB0D_0000_1000_8000_00805F9B34FB);

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
                move |new_value, _req| {
                    let state = state.clone();
                    let event_tx = event_tx.clone();
                    Box::pin(async move {
                        let token_attempt =
                            String::from_utf8_lossy(&new_value).trim().to_string();
                        let state_guard = state.lock().await;

                        // Phone is talking to us → reset the idle timer
                        // before doing anything else. We bump on every op
                        // regardless of auth outcome (matches spec: "no
                        // FB01–FB0x reads/writes for 5 minutes → exit").
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);

                        if crate::libs::ble::gatt::auth::verify_token(
                            &token_attempt,
                            &state_guard.provisioning_session,
                        ) {
                            state_guard.authenticated.store(true, Ordering::SeqCst);
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
                move |_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        let is_auth = state_guard.authenticated.load(Ordering::SeqCst);
                        let response =
                            crate::libs::ble::gatt::auth::auth_response(is_auth);
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
                move |_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.authenticated.load(Ordering::SeqCst) {
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
                move |new_value, _req| {
                    let state = state.clone();
                    let event_tx = event_tx.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.authenticated.load(Ordering::SeqCst) {
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
                move |_new_value, _req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.authenticated.load(Ordering::SeqCst) {
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
                move |_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.authenticated.load(Ordering::SeqCst) {
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
                move |_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.authenticated.load(Ordering::SeqCst) {
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
                move |new_value, _req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.authenticated.load(Ordering::SeqCst) {
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

    // --- Sticker Add characteristic (FB0D) ------------------------------------
    // Write {"deveui","joineui","appkey","name","serial_number"} to enroll a
    // LoRaWAN sticker (OTAA) into the local ChirpStack via the shared add path
    // (same as MQTT's AddLoRaWANSticker). Read returns the structured result of
    // the most recent write. Auth-gated, mirrors FB09/FB01.
    let sticker_add_char = Characteristic {
        uuid: STICKER_ADD_CHAR_UUID.into(),
        write: Some(CharacteristicWrite {
            write: true,
            method: CharacteristicWriteMethod::Fun(Box::new({
                let state = state.clone();
                move |new_value, _req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.authenticated.load(Ordering::SeqCst) {
                            return Err(ReqError::NotAuthorized);
                        }
                        let deps = state_guard.sticker_deps();
                        drop(state_guard);

                        let req: crate::libs::ble::gatt::sticker::StickerAddRequest =
                            match serde_json::from_slice(&new_value) {
                                Ok(r) => r,
                                Err(_) => {
                                    // Record the failure so the FB0D read does
                                    // not surface a stale prior result.
                                    crate::libs::ble::gatt::sticker::set_last_result(
                                        crate::libs::ble::gatt::sticker::StickerAddResponse {
                                            success: false,
                                            message: "invalid json".to_string(),
                                            deveui: String::new(),
                                        },
                                    );
                                    return Err(ReqError::InvalidValueLength);
                                }
                            };

                        let prepared = match crate::libs::ble::gatt::sticker::prepare(&req) {
                            Ok(p) => p,
                            Err(msg) => {
                                crate::libs::ble::gatt::sticker::set_last_result(
                                    crate::libs::ble::gatt::sticker::StickerAddResponse {
                                        success: false,
                                        message: msg,
                                        deveui: req.deveui.trim().to_lowercase(),
                                    },
                                );
                                return Err(ReqError::Failed);
                            }
                        };

                        // add_lorawan_sticker drives ChirpStack gRPC + disk
                        // writes synchronously — offload it so the BLE worker
                        // thread is not blocked (mirrors the LAN FB09 fix).
                        let dev_eui = prepared.dev_eui.clone();
                        let add_result = tokio::task::spawn_blocking(move || {
                            crate::libs::lorawan::add_lorawan_sticker(
                                &deps,
                                prepared.dev_eui,
                                prepared.name,
                                prepared.serial_number,
                                prepared.activation,
                            )
                        })
                        .await
                        .unwrap_or_else(|_| Err("internal task error".to_string()));

                        let (success, message) = match &add_result {
                            Ok(()) => (true, "sticker enrolled".to_string()),
                            Err(e) => (false, e.clone()),
                        };
                        crate::libs::ble::gatt::sticker::set_last_result(
                            crate::libs::ble::gatt::sticker::StickerAddResponse {
                                success,
                                message,
                                deveui: dev_eui,
                            },
                        );
                        add_result.map_err(|_| ReqError::Failed)
                    })
                }
            })),
            ..Default::default()
        }),
        read: Some(CharacteristicRead {
            read: true,
            fun: Box::new({
                let state = state.clone();
                move |_req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let state_guard = state.lock().await;
                        crate::libs::network::touch_shared(&state_guard.provisioning_session);
                        if !state_guard.authenticated.load(Ordering::SeqCst) {
                            return Err(ReqError::NotAuthorized);
                        }
                        drop(state_guard);
                        let resp = crate::libs::ble::gatt::sticker::last_result();
                        Ok(serde_json::to_vec(&resp).unwrap_or_default())
                    })
                }
            }),
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
                move |new_value, _req| {
                    let state = state.clone();
                    Box::pin(async move {
                        let command =
                            String::from_utf8_lossy(&new_value).trim().to_string();

                        // Fetch auth flag + shared handles without holding the lock
                        // across the slow shell-spawn path.
                        let (is_authenticated, notifier_opt, shell_opt) = {
                            let state_guard = state.lock().await;
                            crate::libs::network::touch_shared(&state_guard.provisioning_session);
                            (
                                state_guard.authenticated.load(Ordering::SeqCst),
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
                                            state_guard.shell_process =
                                                Some(shell_arc.clone());
                                        }
                                        eprintln!("[Terminal] Shell initialized and stored");
                                        shell_arc
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "[Terminal] Failed to spawn shell: {}",
                                            e
                                        );
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
                            if let Err(e) =
                                shell_guard.stdin.write_all(cmd.as_bytes()).await
                            {
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
                        eprintln!(
                            "[Terminal] Client subscribed to RX notifications"
                        );
                        {
                            let mut state_guard = state.lock().await;
                            crate::libs::network::touch_shared(&state_guard.provisioning_session);
                            state_guard.terminal_notifier =
                                Some(Arc::new(Mutex::new(notifier)));
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
        sticker_add_char,
    ];

    if enable_terminal {
        chars.push(terminal_tx_char);
        chars.push(terminal_rx_char);
    }

    let fiber_service = Service {
        uuid: FIBER_SERVICE_UUID.into(),
        primary: true,
        characteristics: chars,
        ..Default::default()
    };

    Ok(Application {
        services: vec![fiber_service],
        ..Default::default()
    })
}
