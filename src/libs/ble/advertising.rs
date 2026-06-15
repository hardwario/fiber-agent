use std::process::{Command, Stdio};

/// Hook from the button monitor: the user just held UP and entered the
/// QR/pairing screen. Turn the LE advertisement ON so the phone can find
/// the device for the duration of the QR session.
///
/// Outside this window the device deliberately stays invisible to BLE
/// scans — that way the Android "Pair" dialog never surfaces ambiently;
/// it can only appear during an explicit pairing flow that the user
/// already initiated on the device. Service UUID is hard-coded to FIBER's
/// FB00 so the call site doesn't need to import the constant.
pub fn start_ble_advertising() -> Result<(), String> {
    start_persistent_advertising(FIBER_SERVICE_UUID_STR)
}

/// Hook from the button monitor: the QR session has ended (user cancelled,
/// session timed out, or user navigated away). Tear the LE advertisement
/// down so the device returns to invisible state.
pub fn stop_ble_advertising() -> Result<(), String> {
    stop_persistent_advertising()
}

/// FIBER GATT service UUID as a string literal. The canonical
/// [`crate::libs::ble::gatt::service::FIBER_SERVICE_UUID`] is a
/// `uuid::Uuid` constant; this string mirror exists so this module
/// doesn't have to depend on the gatt::service module.
const FIBER_SERVICE_UUID_STR: &str = "0000fb00-0000-1000-8000-00805f9b34fb";

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

/// Quote a single argv element for safe inclusion in a `sh -c` command
/// string. Wraps in single quotes; any embedded single quotes are
/// terminated, escaped, and reopened (the standard POSIX idiom).
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Runs `btmgmt <args>` via `sh -c "exec timeout 5 btmgmt ARGS </dev/null"`.
///
/// Why the shell wrapper instead of `Command::new("btmgmt")`:
///   - btmgmt's underlying bt_shell readline loop waits forever on stdin
///     when invoked non-interactively if stdin is not at EOF. The shell
///     redirect `< /dev/null` guarantees an immediately-closed stdin
///     before `exec` hands off to btmgmt — equivalent to the manual
///     `btmgmt ... < /dev/null` command that the user verified works.
///   - Rust's `Stdio::null()` *should* be equivalent but in practice the
///     systemd-spawned fiber.service still wedges btmgmt; routing through
///     a fresh shell process with an explicit redirect side-steps any
///     fd-inheritance subtlety.
///   - `exec` makes the shell replace itself with timeout, so the
///     process tree stays clean (no leftover sh wrapper).
///   - `timeout 5` is a defense in depth: if btmgmt STILL manages to
///     hang, we get our worker thread back after 5 s with exit 124.
fn run_btmgmt(args: &[&str]) -> Result<(), String> {
    let quoted_args: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
    let cmd = format!("exec timeout 5 btmgmt {} </dev/null", quoted_args.join(" "));

    let output = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to execute btmgmt {}: {}", args.join(" "), e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);
        if code == 124 {
            return Err(format!("btmgmt {} timed out after 5s", args.join(" ")));
        }
        return Err(format!("btmgmt {} failed (exit {}): {}", args.join(" "), code, stderr));
    }
    Ok(())
}
