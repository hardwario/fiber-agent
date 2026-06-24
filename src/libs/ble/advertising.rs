//! Button-flow hooks kept as no-ops.
//!
//! Earlier iterations of the FIBER firmware drove BLE advertising from
//! the button monitor by shelling out to `btmgmt`. That path turned out
//! to be very fragile against bluez 5.72 + BCM4345C0: external `btmgmt`
//! commands wedge for ≥5 s once fiber.service is running, blocking the
//! button handler and stalling the QR screen transition.
//!
//! Advertising is now owned entirely by the BleMonitor via `bluer`
//! (see `crate::libs::ble::gatt::run_server`) — it brings the
//! advertisement up alongside the GATT app at process start and tears
//! it down at shutdown. The button monitor no longer needs to manage
//! advertising state; these stubs exist only so the call sites in
//! `crate::libs::display::buttons` stay compileable and audit-friendly.

pub fn start_ble_advertising() -> Result<(), String> {
    eprintln!("[BLE] start_ble_advertising: no-op (BleMonitor owns advert lifecycle)");
    Ok(())
}

pub fn stop_ble_advertising() -> Result<(), String> {
    eprintln!("[BLE] stop_ble_advertising: no-op (BleMonitor owns advert lifecycle)");
    Ok(())
}
