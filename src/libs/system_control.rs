//! Shared device-teardown machinery: signed-command reboot/power-off
//! (`libs::mqtt::monitor`) and the front-panel Reboot/Shutdown menu
//! (`libs::display::buttons`) both call into this module rather than
//! duplicating the audit + `systemctl` invocation.
//!
//! **Shutdown asymmetry, by design:** the front-panel "Shutdown" menu item
//! runs a genuine `systemctl poweroff` through [`execute_teardown`]. The
//! remote, Ed25519-signed `PowerOffDevice` MQTT command does **not** — it
//! calls `execute_standby` in `libs::mqtt::monitor` instead, entering deep
//! standby, because nothing on this board can wake a *halted* CM4 (see
//! `libs::power::standby`). A shutdown requested at the panel implies a human
//! is standing next to the device who can physically power-cycle it; a
//! shutdown requested over the network does not, so it must not strand the
//! device off.

use std::time::Duration;

/// Sentinel `requested_by` for actions triggered at the physical front
/// panel, bypassing the Ed25519 signed-command channel entirely (physical
/// access is treated as implicit authorization for these actions) — but
/// still audited, and tagged distinctly from any MQTT-signer identity, so a
/// reviewer can tell a local reboot/shutdown apart from a remote one at a
/// glance.
pub const LOCAL_BUTTON_REQUESTER: &str = "local:button";

/// How long to wait for the encrypted audit row of a teardown command (reboot
/// or power-off) to reach disk. Bounded on purpose: the operator's signed intent
/// outranks a perfect audit trail, so a wedged storage thread degrades to a WARN
/// instead of leaving the device up forever.
const TEARDOWN_AUDIT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Grace window between returning from a teardown executor and actually going
/// down, so the queued SUCCESS ack gets on the wire. See `execute_teardown` for
/// why this cannot be replaced by waiting on the child process.
const TEARDOWN_GRACE: Duration = Duration::from_millis(1500);

/// How long to wait for the display thread to confirm the panel is dark. Fits
/// inside [`TEARDOWN_GRACE`] and is an order of magnitude above the display
/// loop's 50 ms tick, so a running display always makes it; an absent or wedged
/// one degrades to a WARN rather than holding the device up.
pub(crate) const DISPLAY_BLANK_TIMEOUT: Duration = Duration::from_millis(500);

/// Write the authorization record for a command that interrupts monitoring,
/// and block until it is durable.
///
/// Shared by reboot, power-off and standby. `label` is only used for log
/// prefixes.
///
/// Never fails the caller: an unaudited teardown is bad, but refusing to act
/// on a signed command (or a physically-authorized button press) because the
/// audit database is unhappy would leave a device that cannot be stopped at
/// all.
pub fn audit_and_flush(
    label: &str,
    audit_event: &'static str,
    reason: &str,
    requested_by: &str,
    storage_handle: &Option<crate::libs::storage::StorageHandle>,
) {
    let Some(storage) = storage_handle else {
        eprintln!("[system_control] WARN: no storage handle — {label} will not be audited");
        return;
    };

    let details = format!(
        r#"{{"reason":{},"requested_by":{}}}"#,
        serde_json::Value::String(reason.to_string()),
        serde_json::Value::String(requested_by.to_string()),
    );
    if let Err(e) = storage.log_audit_event(
        audit_event.to_string(),
        Some("audit_log".to_string()),
        Some(details),
    ) {
        eprintln!("[system_control] WARN: failed to queue {label} audit row: {e}");
    }
    // The storage worker is a single thread draining one FIFO channel,
    // so a FlushSync reply also proves the audit row queued above was
    // committed and checkpointed. It is also what keeps unwritten
    // temperature samples from being lost across the restart.
    if let Err(e) = storage.flush_sync(TEARDOWN_AUDIT_FLUSH_TIMEOUT) {
        eprintln!("[system_control] WARN: {label} audit row may not be durable: {e}");
    }
}

/// `verb` is the systemctl subcommand ("reboot"/"poweroff") and doubles as
/// the worker-thread name and log prefix.
pub fn execute_teardown(
    verb: &'static str,
    audit_event: &'static str,
    reason: String,
    requested_by: String,
    storage_handle: &Option<crate::libs::storage::StorageHandle>,
) -> Result<(), String> {
    eprintln!(
        "[system_control] Device {} requested by {}: {}",
        verb, requested_by, reason
    );

    audit_and_flush(verb, audit_event, &reason, &requested_by, storage_handle);

    // Spawn and return immediately; do NOT wait on the child here. For the
    // MQTT-signed path, the SUCCESS ack is only *queued* at this point and
    // needs this function to return before it reaches the wire (see the
    // original call site for the full explanation). `--no-block` for the
    // same reason inside the thread: without it, systemctl waits for the
    // job to finish and may never return.
    std::thread::Builder::new()
        .name(verb.to_string())
        .spawn(move || {
            // Blank inside the grace window rather than extending it: the
            // panel goes dark while the ack is still on its way out, so the
            // device stops showing readings it is no longer taking. The
            // deadline keeps the ack's full window intact no matter how fast
            // the display confirms.
            let deadline = std::time::Instant::now() + TEARDOWN_GRACE;
            crate::libs::display::blank::request_blank();
            if !crate::libs::display::blank::wait_until_blank(DISPLAY_BLANK_TIMEOUT) {
                eprintln!("[{verb}] WARN: display did not confirm blank — continuing anyway");
            }
            std::thread::sleep(deadline.saturating_duration_since(std::time::Instant::now()));
            match std::process::Command::new("systemctl")
                .args([verb, "--no-block"])
                .status()
            {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    // A device that stays up must not stay dark.
                    crate::libs::display::blank::cancel_blank();
                    eprintln!("[{verb}] systemctl {verb} exited with {status} — device stays up");
                }
                Err(e) => {
                    crate::libs::display::blank::cancel_blank();
                    eprintln!("[{verb}] failed to execute systemctl {verb}: {e} — device stays up");
                }
            }
        })
        .map_err(|e| format!("Failed to spawn {verb} thread: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_and_flush_without_storage_handle_does_not_panic() {
        audit_and_flush("reboot", "REBOOT", "test", LOCAL_BUTTON_REQUESTER, &None);
    }
}
