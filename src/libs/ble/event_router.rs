//! BLE event router thread.
//!
//! Drains BleEvents from the BleHandle and dispatches them to display
//! state and the pairing handle. This is the only place where BLE events
//! touch the rest of the application — keeping the integration explicit
//! and reviewable.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::gatt::{BleEvent, BleHandle};
use crate::libs::display::SharedDisplayStateHandle;
use crate::libs::pairing::PairingHandle;

pub struct BleEventRouter {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl Drop for BleEventRouter {
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread_handle.take() {
            let _ = h.join();
        }
    }
}

pub fn spawn_ble_event_router(
    ble: BleHandle,
    display: SharedDisplayStateHandle,
    pairing: Option<PairingHandle>,
) -> BleEventRouter {
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown_flag.clone();

    let thread_handle = thread::Builder::new()
        .name("ble-event-router".to_string())
        .spawn(move || router_loop(ble, display, pairing, shutdown_clone))
        .expect("failed to spawn ble-event-router thread");

    BleEventRouter {
        thread_handle: Some(thread_handle),
        shutdown_flag,
    }
}

fn router_loop(
    ble: BleHandle,
    display: SharedDisplayStateHandle,
    pairing: Option<PairingHandle>,
    shutdown: Arc<AtomicBool>,
) {
    eprintln!("[BleEventRouter] Started");
    while !shutdown.load(Ordering::Relaxed) {
        match ble.try_recv_event() {
            Some(ev) => handle(&ev, &display, pairing.as_ref()),
            None => thread::sleep(Duration::from_millis(50)),
        }
    }
    eprintln!("[BleEventRouter] Exited");
}

fn handle(ev: &BleEvent, display: &SharedDisplayStateHandle, pairing: Option<&PairingHandle>) {
    match ev {
        BleEvent::ClientConnected { addr } => {
            if let Some(p) = pairing {
                p.cancel_pairing(); // ensure MQTT pairing screen exits if it was showing
                p.set_ble_active(true);
            }
            if let Ok(mut d) = display.lock() {
                // A client connecting is a notification, not a navigation. If the
                // operator is reading something they opened at the panel, leave it
                // alone — the connection is logged either way.
                if d.current_screen.is_operator_owned() {
                    eprintln!(
                        "[BleEventRouter] Client connected ({}) — leaving the operator's screen up",
                        addr
                    );
                } else {
                    d.show_ble_connected(addr);
                }
            }
        }
        BleEvent::ClientDisconnected => {
            if let Some(p) = pairing {
                p.set_ble_active(false);
            }
            if let Ok(mut d) = display.lock() {
                // Only dismiss our own screen. Returning to the overview from
                // anything else would close a screen this router never opened.
                if d.current_screen.is_ble_screen() {
                    d.show_sensor_overview();
                }
            }
        }
        BleEvent::AuthSuccess | BleEvent::AuthFailed => {
            // No display change for auth alone — keep the BLE Connected screen.
        }
        BleEvent::WifiConnecting { ssid } => {
            if let Ok(mut d) = display.lock() {
                d.show_ble_provisioning(ssid);
            }
        }
        BleEvent::WifiConnected { ssid, ip } => {
            if let Ok(mut d) = display.lock() {
                d.show_ble_wifi_ok(ssid, ip);
            }
        }
        BleEvent::WifiFailed { error } => {
            if let Ok(mut d) = display.lock() {
                d.show_ble_wifi_fail(error);
            }
        }
        BleEvent::LanConfigured { mode, ip } => {
            // LCD feedback for LAN is a follow-up; log only for now.
            eprintln!("[BleEventRouter] LAN configured: mode={} ip={}", mode, ip);
        }
        BleEvent::LanFailed { error } => {
            eprintln!("[BleEventRouter] LAN configuration failed: {}", error);
        }
    }
}

#[cfg(test)]
mod screen_ownership_tests {
    use super::*;
    use crate::libs::display::{DisplayState, Screen};
    use std::sync::Mutex;

    fn display_showing(f: impl FnOnce(&mut DisplayState)) -> SharedDisplayStateHandle {
        let handle = Arc::new(Mutex::new(DisplayState::new()));
        f(&mut handle.lock().unwrap());
        handle
    }

    #[test]
    fn a_client_connect_does_not_steal_the_system_info_screen() {
        // An operator held DOWN for 2s to read system info. A BLE client
        // connecting is not a reason to take that away.
        let display = display_showing(|d| d.show_system_info());

        handle(
            &BleEvent::ClientConnected {
                addr: "AA:BB:CC:DD:EE:01".to_string(),
            },
            &display,
            None,
        );

        assert!(matches!(
            display.lock().unwrap().current_screen,
            Screen::SystemInfo { .. }
        ));
    }

    #[test]
    fn a_client_connect_does_not_steal_the_local_action_menu() {
        let display = display_showing(|d| d.show_menu());

        handle(
            &BleEvent::ClientConnected {
                addr: "AA:BB:CC:DD:EE:01".to_string(),
            },
            &display,
            None,
        );

        assert!(matches!(
            display.lock().unwrap().current_screen,
            Screen::Menu { .. }
        ));
    }

    #[test]
    fn a_client_connect_over_the_idle_overview_still_shows_the_ble_screen() {
        // The useful half of the behaviour must survive the fix.
        let display = display_showing(|d| d.show_sensor_overview());

        handle(
            &BleEvent::ClientConnected {
                addr: "AA:BB:CC:DD:EE:01".to_string(),
            },
            &display,
            None,
        );

        assert!(matches!(
            display.lock().unwrap().current_screen,
            Screen::BleConnected { .. }
        ));
    }

    #[test]
    fn a_disconnect_returns_to_the_overview_from_a_ble_screen() {
        let display = display_showing(|d| d.show_ble_connected("AA:BB:CC:DD:EE:01"));

        handle(&BleEvent::ClientDisconnected, &display, None);

        assert!(matches!(
            display.lock().unwrap().current_screen,
            Screen::SensorOverview { .. }
        ));
    }

    #[test]
    fn a_disconnect_does_not_close_the_operators_system_info_screen() {
        // The reported defect: a device ageing out of the scan cache closed a
        // screen the button thread has no timeout for, so it looked like the
        // panel exited by itself.
        let display = display_showing(|d| d.show_system_info());

        handle(&BleEvent::ClientDisconnected, &display, None);

        assert!(
            matches!(
                display.lock().unwrap().current_screen,
                Screen::SystemInfo { .. }
            ),
            "a BLE disconnect must only dismiss a BLE screen"
        );
    }
}
