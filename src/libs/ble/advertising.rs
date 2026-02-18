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
