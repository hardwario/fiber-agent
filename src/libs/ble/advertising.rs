use std::process::Command;

/// Inicia BLE advertising para pareamento
pub fn start_ble_advertising() -> Result<(), String> {
    eprintln!("[BLE] Starting advertising...");
    run_btmgmt(&["power", "off"])?;
    run_btmgmt(&["le", "on"])?;
    run_btmgmt(&["power", "on"])?;
    run_btmgmt(&["add-adv", "-g", "1"])?;
    eprintln!("[BLE] Advertising started successfully");
    Ok(())
}

/// Para BLE advertising (conexões existentes permanecem)
pub fn stop_ble_advertising() -> Result<(), String> {
    eprintln!("[BLE] Stopping advertising...");
    run_btmgmt(&["remove-adv", "1"])?;
    eprintln!("[BLE] Advertising stopped");
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
