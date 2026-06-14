use std::process::Command;

/// Legacy pairing-flow hook called by the button monitor when the user
/// holds UP to enter the QR/pairing screen.
///
/// Previously this function powered the controller off, re-enabled LE, and
/// powered it back on before re-adding a generic advertisement — needed
/// when there was no in-app GATT server running. With the BleMonitor now
/// maintaining a connectable persistent advertisement at all times (see
/// [`start_persistent_advertising`]), the controller is already advertising
/// the GATT service: a pairing UI transition needs no MGMT plumbing.
///
/// The function is kept (as a no-op) instead of removed from
/// [`crate::libs::display::buttons`] so the call sites stay symmetric with
/// [`stop_ble_advertising`] and so any pairing-token security invariants
/// the caller relies on aren't disturbed.
pub fn start_ble_advertising() -> Result<(), String> {
    eprintln!("[BLE] start_ble_advertising: no-op (BleMonitor maintains persistent advert)");
    Ok(())
}

/// Symmetric counterpart to [`start_ble_advertising`]. Tearing down the
/// pairing session is owned by the caller (the provisioning_session slot is
/// cleared in `buttons.rs`); leaving the advertisement up is fine because
/// pairing is gated by the FB01 auth characteristic + PIN, not by adv
/// visibility.
pub fn stop_ble_advertising() -> Result<(), String> {
    eprintln!("[BLE] stop_ble_advertising: no-op (BleMonitor owns advert lifecycle)");
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
    let _ = run_btmgmt(&["rm-adv", "1"]);
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
    let _ = run_btmgmt(&["rm-adv", "1"]);
    Ok(())
}

/// Runs `btmgmt <args>` via the `timeout(1)` wrapper so a stuck/hung btmgmt
/// process (observed with `rm-adv` against a non-existent instance when run
/// non-interactively from the fiber.service cgroup) cannot wedge the worker
/// thread waiting on `.output()`. The 5-second cap is well above any normal
/// btmgmt round-trip (<100 ms) so it only fires on a real hang.
fn run_btmgmt(args: &[&str]) -> Result<(), String> {
    let mut argv: Vec<&str> = vec!["5", "btmgmt"];
    argv.extend(args.iter().copied());
    let output = Command::new("timeout")
        .args(&argv)
        .output()
        .map_err(|e| format!("Failed to execute timeout 5 btmgmt {}: {}", args.join(" "), e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // timeout(1) exits 124 when the command timed out. Surface that
        // distinctly so log readers know the wrapper kicked in.
        let code = output.status.code().unwrap_or(-1);
        if code == 124 {
            return Err(format!("btmgmt {} timed out after 5s", args.join(" ")));
        }
        return Err(format!("btmgmt {} failed: {}", args.join(" "), stderr));
    }
    Ok(())
}
