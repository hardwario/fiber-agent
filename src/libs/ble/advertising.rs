use std::process::Command;
use std::thread;
use std::time::Duration;

/// Inicia BLE advertising para pareamento
/// Runs commands in background thread but waits enough time for completion
pub fn start_ble_advertising() -> Result<(), String> {
    eprintln!("[BLE] Starting advertising...");

    thread::spawn(|| {
        if let Err(e) = run_btmgmt(&["power", "off"]) {
            eprintln!("[BLE] Failed to power off: {}", e);
        }
        if let Err(e) = run_btmgmt(&["le", "on"]) {
            eprintln!("[BLE] Failed to enable LE: {}", e);
        }
        if let Err(e) = run_btmgmt(&["power", "on"]) {
            eprintln!("[BLE] Failed to power on: {}", e);
        }
        if let Err(e) = run_btmgmt(&["add-adv", "-g", "1"]) {
            eprintln!("[BLE] Failed to add advertisement: {}", e);
        }
        eprintln!("[BLE] Advertising started successfully");
    });

    // Wait for BLE commands to complete before returning
    thread::sleep(Duration::from_secs(3));

    Ok(())
}

/// Para BLE advertising (non-blocking)
pub fn stop_ble_advertising() -> Result<(), String> {
    eprintln!("[BLE] Stopping advertising...");
    thread::spawn(|| {
        if let Err(e) = run_btmgmt(&["remove-adv", "1"]) {
            eprintln!("[BLE] Failed to remove advertisement: {}", e);
        }
        eprintln!("[BLE] Advertising stopped");
    });
    Ok(())
}

/// Register a persistent LE advertisement that announces the in-app GATT
/// service UUID. Uses btmgmt's `add-adv` (MGMT_OP_ADD_ADVERTISING, 0x003E)
/// — the legacy advertising path — bypassing bluer/bluez 5.72's extended
/// advertising selection, which fails on BT 4.2-only controllers like the
/// BCM4345C0 on the Pi CM4 (kernel returns Invalid Parameters on the
/// MGMT_OP_ADD_EXT_ADV_DATA command).
///
/// Unlike [`start_ble_advertising`] (the button-press pairing flow), this
/// does NOT power-cycle the controller — at the point this is called the
/// GATT app has already been registered with bluez and a power cycle would
/// disrupt that. Synchronous; expected to complete in <100 ms.
pub fn start_persistent_advertising(service_uuid: &str) -> Result<(), String> {
    eprintln!("[BLE] Registering persistent LE advertisement for service {}", service_uuid);
    // Idempotent: clear any previous instance 1 before adding a new one.
    let _ = run_btmgmt(&["remove-adv", "1"]);
    run_btmgmt(&[
        "add-adv",
        "-c",                 // connectable (peers can open GATT)
        "-g",                 // general discoverable
        "-u", service_uuid,   // include service UUID in adv data
        "-n",                 // include controller name in scan response
        "1",                  // instance ID
    ])?;
    eprintln!("[BLE] Persistent LE advertisement registered (instance 1)");
    Ok(())
}

/// Remove the persistent advertisement registered by
/// [`start_persistent_advertising`]. Synchronous and idempotent.
pub fn stop_persistent_advertising() -> Result<(), String> {
    eprintln!("[BLE] Removing persistent LE advertisement");
    let _ = run_btmgmt(&["remove-adv", "1"]);
    Ok(())
}

fn run_btmgmt(args: &[&str]) -> Result<(), String> {
    let output = Command::new("btmgmt")
        .args(args)
        .output()
        .map_err(|e| format!("Failed to execute btmgmt {}: {}", args.join(" "), e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("btmgmt {} failed: {}", args.join(" "), stderr));
    }
    Ok(())
}
