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
                p.cancel_pairing();      // ensure MQTT pairing screen exits if it was showing
                p.set_ble_active(true);
            }
            if let Ok(mut d) = display.lock() {
                d.show_ble_connected(addr);
            }
        }
        BleEvent::ClientDisconnected => {
            if let Some(p) = pairing {
                p.set_ble_active(false);
            }
            if let Ok(mut d) = display.lock() {
                d.show_sensor_overview();
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
    }
}
