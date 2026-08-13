//! Taking the device down: the audit-then-teardown machinery shared by every
//! command that interrupts monitoring.
//!
//! Reboot, power-off and factory reset all have to do the same two things in the
//! same order before the process stops existing:
//!
//! 1. Get the authorization record onto disk, durably. The `/tmp/fiber_audit.db`
//!    authorization record does not survive — the unit runs with
//!    `PrivateTmp=true`, so a reboot wipes it exactly as a power-off does. The
//!    row in the encrypted, hash-chained `audit_log` is therefore the only
//!    surviving evidence that the gap in monitoring was authorized and by whom,
//!    which is why [`audit_and_flush`] waits for it to land.
//! 2. Blank the panel and then actually go down, without blocking the caller —
//!    see [`spawn_teardown`] for why the wait has to happen on another thread.
//!
//! This module holds that logic once so the MQTT command executor, the
//! front-panel button path and the factory-reset flow do not each carry their
//! own copy of it. [`execute_teardown`] is the both-halves-in-one-call form.
//!
//! Nothing here is unit-testable end to end on purpose: [`spawn_teardown`]
//! finishes by running `systemctl <verb>`, so a test that exercised it would
//! reboot the machine running the test suite. What is testable — the audit row,
//! the never-fail-the-caller contract, and the timing budget the grace window
//! depends on — is covered below.

use std::time::Duration;

/// `requested_by` for an action triggered from the device's own front panel.
///
/// The button path bypasses the signed-command channel entirely: whoever is
/// holding the buttons has physical possession of the device, which is
/// authorization enough for the actions the panel offers. It is still audited,
/// and tagged with this sentinel so an auditor can tell it apart at a glance
/// from any MQTT signer identity — no signer key can ever produce this string,
/// because signer identities are certificate subjects.
pub const LOCAL_BUTTON_REQUESTER: &str = "local:button";

/// How long to wait for the encrypted audit row of a teardown command (reboot
/// or power-off) to reach disk. Bounded on purpose: the operator's signed intent
/// outranks a perfect audit trail, so a wedged storage thread degrades to a WARN
/// instead of leaving the device up forever.
pub const TEARDOWN_AUDIT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Grace window between returning from a teardown executor and actually going
/// down, so the queued SUCCESS ack gets on the wire. See [`spawn_teardown`] for
/// why this cannot be replaced by waiting on the child process.
pub const TEARDOWN_GRACE: Duration = Duration::from_millis(1500);

/// How long to wait for the display thread to confirm the panel is dark. Fits
/// inside [`TEARDOWN_GRACE`] and is an order of magnitude above the display
/// loop's 50 ms tick, so a running display always makes it; an absent or wedged
/// one degrades to a WARN rather than holding the device up.
pub const DISPLAY_BLANK_TIMEOUT: Duration = Duration::from_millis(500);

/// Write the authorization record for a command that interrupts monitoring,
/// and block until it is durable.
///
/// Shared by reboot, standby and factory reset. `label` is only used for log
/// prefixes.
///
/// Never fails the caller: an unaudited teardown is bad, but refusing to act
/// on a signed command because the audit database is unhappy would leave a
/// device that cannot be stopped at all.
pub fn audit_and_flush(
    label: &str,
    audit_event: &'static str,
    reason: &str,
    requested_by: &str,
    storage_handle: &Option<crate::libs::storage::StorageHandle>,
) {
    let Some(storage) = storage_handle else {
        eprintln!("[MQTT Monitor] WARN: no storage handle — {label} will not be audited");
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
        eprintln!("[MQTT Monitor] WARN: failed to queue {label} audit row: {e}");
    }
    // The storage worker is a single thread draining one FIFO channel,
    // so a FlushSync reply also proves the audit row queued above was
    // committed and checkpointed. It is also what keeps unwritten
    // temperature samples from being lost across the restart.
    if let Err(e) = storage.flush_sync(TEARDOWN_AUDIT_FLUSH_TIMEOUT) {
        eprintln!("[MQTT Monitor] WARN: {label} audit row may not be durable: {e}");
    }
}

/// Blank the panel and hand the device to systemd, on a thread of its own.
///
/// `verb` is the systemctl subcommand ("reboot"/"poweroff") and doubles as the
/// worker-thread name and log prefix.
///
/// Returns as soon as the thread is spawned; it does NOT wait on the child. The
/// SUCCESS ack for a teardown command is only *queued* when this is called:
/// `AsyncClient::publish` hands the packet to rumqttc's channel and it reaches
/// the socket the next time `eventloop.poll()` runs — which cannot happen until
/// the caller returns, because the whole ConfigConfirm branch runs inside the
/// poll arm of a `tokio::select!`. Blocking here would mean the signer never
/// learns the command worked. `--no-block` is passed for the same reason inside
/// the thread: without it, systemctl waits for the job to finish and may never
/// return.
///
/// The only error case is a failure to spawn the thread — once spawned, a
/// systemctl that refuses is logged and un-blanks the panel rather than
/// propagating, because by then the caller has long since returned.
pub fn spawn_teardown(verb: &'static str) -> Result<(), String> {
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

/// Audit the request, then take the device down — the whole teardown in one
/// call, for callers that want both halves and nothing in between.
///
/// A caller that has work to do *after* the audit row is durable but *before*
/// the device goes down (wiping data, for one) composes [`audit_and_flush`] and
/// [`spawn_teardown`] itself instead.
pub fn execute_teardown(
    verb: &'static str,
    audit_event: &'static str,
    reason: String,
    requested_by: String,
    storage_handle: &Option<crate::libs::storage::StorageHandle>,
) -> Result<(), String> {
    eprintln!(
        "[MQTT Monitor] Device {} requested by {}: {}",
        verb, requested_by, reason
    );

    audit_and_flush(verb, audit_event, &reason, &requested_by, storage_handle);
    spawn_teardown(verb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::storage::{db::Database, StorageThread};

    /// The never-fail-the-caller contract, in its most brutal form: a device with
    /// no storage at all still has to be stoppable. This must log and return, not
    /// unwrap on the missing handle.
    #[test]
    fn audit_and_flush_without_storage_handle_does_not_panic() {
        audit_and_flush("reboot", "REBOOT", "no db here", "dr.jane", &None);
    }

    /// The audit row is the only surviving evidence a teardown was authorized, so
    /// assert on what it actually contains — the event name the reviewer greps for
    /// and both fields of the details JSON — and that `flush_sync` made it durable
    /// *without* a shutdown/join, which is what a real reboot never gets to do.
    #[test]
    fn audit_and_flush_writes_a_durable_row_naming_the_requester() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        let (handle, join) = StorageThread::spawn(&path, 1).unwrap();

        audit_and_flush(
            "reboot",
            "REBOOT",
            "firmware update",
            "dr.jane",
            &Some(handle.clone()),
        );

        let conn = Database::new(&path, 1).unwrap().connect().unwrap();
        let details: String = conn
            .query_row(
                "SELECT details FROM audit_log WHERE operation = 'REBOOT'",
                [],
                |r| r.get(0),
            )
            .expect("REBOOT audit row was not durable after audit_and_flush");
        assert_eq!(
            details, r#"{"reason":"firmware update","requested_by":"dr.jane"}"#,
            "audit details JSON changed shape"
        );

        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    /// A reason or signer identity carrying a quote would produce invalid JSON if
    /// the details string were built by naive concatenation, and an audit row that
    /// cannot be parsed is an audit row that cannot be reviewed.
    #[test]
    fn audit_and_flush_escapes_quotes_in_reason_and_requester() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        let (handle, join) = StorageThread::spawn(&path, 1).unwrap();

        audit_and_flush(
            "poweroff",
            "POWER_OFF",
            r#"said "now""#,
            r#"a\b"#,
            &Some(handle.clone()),
        );

        let conn = Database::new(&path, 1).unwrap().connect().unwrap();
        let details: String = conn
            .query_row(
                "SELECT details FROM audit_log WHERE operation = 'POWER_OFF'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&details)
            .unwrap_or_else(|e| panic!("audit details is not valid JSON: {e} ({details})"));
        assert_eq!(parsed["reason"], r#"said "now""#);
        assert_eq!(parsed["requested_by"], r#"a\b"#);

        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    /// `spawn_teardown` blanks the panel and waits for confirmation *inside* the
    /// grace window, so the display budget has to fit within it. If someone raises
    /// DISPLAY_BLANK_TIMEOUT past TEARDOWN_GRACE, an unconfirmed blank silently
    /// eats the whole window the queued SUCCESS ack needs to reach the wire.
    #[test]
    fn display_blank_budget_fits_inside_the_teardown_grace_window() {
        assert!(
            DISPLAY_BLANK_TIMEOUT < TEARDOWN_GRACE,
            "display blank timeout {DISPLAY_BLANK_TIMEOUT:?} must fit inside grace {TEARDOWN_GRACE:?}"
        );
    }

    /// The sentinel exists to be distinguishable from a certificate-subject signer
    /// identity in the audit log; pin the exact string, because changing it would
    /// silently split the audit history of front-panel actions in two.
    #[test]
    fn local_button_requester_is_a_stable_namespaced_sentinel() {
        assert_eq!(LOCAL_BUTTON_REQUESTER, "local:button");
    }
}
