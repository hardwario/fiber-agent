use std::process::Command;
use std::thread;

/// Inicia BLE advertising para pareamento (non-blocking)
pub fn start_ble_advertising() -> Result<(), String> {
    eprintln!("[BLE] Starting advertising...");
    // Spawn in separate thread to avoid blocking button monitor
    thread::spawn(|| {
        if let Err(e) = run_btmgmt(&["power", "off"]) {
            eprintln!("[BLE] Failed to power off: {}", e);
            return;
        }
        if let Err(e) = run_btmgmt(&["le", "on"]) {
            eprintln!("[BLE] Failed to enable LE: {}", e);
            return;
        }
        if let Err(e) = run_btmgmt(&["power", "on"]) {
            eprintln!("[BLE] Failed to power on: {}", e);
            return;
        }
        if let Err(e) = run_btmgmt(&["add-adv", "-g", "1"]) {
            eprintln!("[BLE] Failed to add advertisement: {}", e);
            return;
        }
        eprintln!("[BLE] Advertising started successfully");
    });
    Ok(())
}

/// Para BLE advertising (non-blocking)
pub fn stop_ble_advertising() -> Result<(), String> {
    eprintln!("[BLE] Stopping advertising...");
    // Spawn in separate thread to avoid blocking button monitor
    thread::spawn(|| {
        if let Err(e) = run_btmgmt(&["remove-adv", "1"]) {
            eprintln!("[BLE] Failed to remove advertisement: {}", e);
            return;
        }
        eprintln!("[BLE] Advertising stopped");
    });
    Ok(())
}

fn run_btmgmt(args: &[&str]) -> Result<(), String> {
    let output = Command::new("btmgmt")
        .args(args)
        .output()
        .map_err(|e| format!("Failed to execute btmgmt {}: {}", args.join(" "), e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("[BLE] btmgmt {} warning: {}", args.join(" "), stderr);
    }
    Ok(())
}
