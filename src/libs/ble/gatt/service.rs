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
                        let pin_attempt =
                            String::from_utf8_lossy(&new_value).trim().to_string();
                        let state_guard = state.lock().await;

                        if crate::libs::ble::gatt::auth::verify_pin(
                            &pin_attempt,
                            &state_guard.pin,
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
                        if !state_guard.authenticated.load(Ordering::SeqCst) {
                            return Err(ReqError::NotAuthorized);
                        }
                        drop(state_guard);

                        let request: crate::libs::ble::gatt::wifi::WiFiConnectRequest =
                            serde_json::from_slice(&new_value)
                                .map_err(|_| ReqError::InvalidValueLength)?;

                        eprintln!("[WiFi] Connecting to '{}'...", request.ssid);
                        let _ = event_tx.try_send(super::BleEvent::WifiConnecting {
                            ssid: request.ssid.clone(),
                        });
                        let result = crate::libs::ble::gatt::wifi::connect_wifi(
                            &request.ssid,
                            &request.password,
                        );

                        if result.connected {
                            eprintln!(
                                "[WiFi] Connected successfully, IP: {}",
                                result.ip_address
                            );
                            let _ = event_tx.try_send(super::BleEvent::WifiConnected {
                                ssid: result.ssid.clone(),
                                ip: result.ip_address.clone(),
                            });
                            Ok(())
                        } else {
                            eprintln!("[WiFi] Connection failed: {}", result.error);
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
                        if !state_guard.authenticated.load(Ordering::SeqCst) {
                            return Err(ReqError::NotAuthorized);
                        }
                        drop(state_guard);

                        eprintln!("[WiFi] Disconnecting...");
                        let result = crate::libs::ble::gatt::wifi::disconnect_wifi();

                        if !result.connected {
                            Ok(())
                        } else {
                            eprintln!("[WiFi] Disconnect failed: {}", result.error);
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
                        // Device info is available without authentication.
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
