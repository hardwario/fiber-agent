//! Time-sync GATT characteristic (FB0B).
//!
//! In a BLE-only deployment the device has no internet, so after a long power
//! loss (PoE) NTP can't fix the clock and the RTC may have drifted/reset. The
//! phone — which has an accurate clock — sets the device time during
//! provisioning: write the current UTC epoch to FB0B, the device applies it
//! (`date -s` + `hwclock --systohc` to persist into the RTC). FB0B read returns
//! the current device time + whether the OS considers the clock synchronized,
//! so the app can decide whether a set is needed.
//!
//! Pure functions (request parse, epoch validation, status serialization) are
//! split from the `Command`-callers so they unit-test without touching the
//! system clock.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Plausible epoch window: 2020-01-01 .. 2100-01-01 (UTC, seconds). Rejects a
/// zeroed / wildly-wrong value so a bad write can't shove the clock to 1970.
const EPOCH_MIN: i64 = 1_577_836_800; // 2020-01-01T00:00:00Z
const EPOCH_MAX: i64 = 4_102_444_800; // 2100-01-01T00:00:00Z

/// FB0B write payload — the phone's current UTC time, seconds since epoch.
#[derive(Clone, Debug, Deserialize)]
pub struct TimeSetRequest {
    pub epoch: i64,
}

/// FB0B read payload — current device clock state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeStatusResponse {
    /// Current device time, seconds since epoch (UTC).
    pub epoch: i64,
    /// Whether the OS reports the clock as synchronized (e.g. NTP).
    pub synchronized: bool,
}

/// Validate a requested epoch is within the plausible window.
pub fn validate_epoch(epoch: i64) -> Result<(), String> {
    if (EPOCH_MIN..=EPOCH_MAX).contains(&epoch) {
        Ok(())
    } else {
        Err(format!("epoch {} out of plausible range [{}, {}]", epoch, EPOCH_MIN, EPOCH_MAX))
    }
}

/// Current system time as a UTC epoch (seconds). Clamps a pre-1970 clock to 0.
pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Whether systemd reports the clock as synchronized (best-effort; false if the
/// query fails or the field is absent).
fn is_synchronized() -> bool {
    Command::new("timedatectl")
        .args(["show", "-p", "NTPSynchronized", "--value"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().eq_ignore_ascii_case("yes"))
        .unwrap_or(false)
}

/// Current device clock state for FB0B read.
pub fn get_time_status() -> TimeStatusResponse {
    TimeStatusResponse { epoch: now_epoch(), synchronized: is_synchronized() }
}

/// Set the system clock to `epoch` (validated) and persist it into the RTC.
///
/// `date -u -s @<epoch>` sets the wall clock; `hwclock --systohc` writes it to
/// the RTC so it survives a reboot. Both need root (the daemon runs as root).
pub fn set_system_time(epoch: i64) -> Result<(), String> {
    validate_epoch(epoch)?;

    let out = Command::new("date")
        .args(["-u", "-s", &format!("@{}", epoch)])
        .output()
        .map_err(|e| format!("spawn date: {}", e))?;
    if !out.status.success() {
        return Err(format!("date failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }

    // Persist to RTC — best-effort: the wall clock is already set, and not
    // every platform has a writable RTC, so a failure here is logged, not fatal.
    match Command::new("hwclock").arg("--systohc").output() {
        Ok(o) if o.status.success() => {}
        Ok(o) => eprintln!(
            "[time] hwclock --systohc failed (wall clock still set): {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => eprintln!("[time] hwclock spawn error (wall clock still set): {}", e),
    }

    eprintln!("[time] system clock set to epoch {}", epoch);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_recent_epoch() {
        assert!(validate_epoch(1_719_403_899).is_ok()); // 2024
        assert!(validate_epoch(EPOCH_MIN).is_ok());
        assert!(validate_epoch(EPOCH_MAX).is_ok());
    }

    #[test]
    fn validate_rejects_out_of_range() {
        assert!(validate_epoch(0).is_err()); // 1970
        assert!(validate_epoch(1_000_000_000).is_err()); // 2001
        assert!(validate_epoch(EPOCH_MIN - 1).is_err());
        assert!(validate_epoch(EPOCH_MAX + 1).is_err());
        assert!(validate_epoch(-5).is_err());
    }

    #[test]
    fn set_system_time_rejects_bad_epoch_before_touching_clock() {
        // Out-of-range never shells out to `date`.
        assert!(set_system_time(0).is_err());
        assert!(set_system_time(-1).is_err());
    }

    #[test]
    fn request_deserializes() {
        let r: TimeSetRequest = serde_json::from_str(r#"{"epoch":1719403899}"#).unwrap();
        assert_eq!(r.epoch, 1_719_403_899);
    }

    #[test]
    fn status_serializes_expected_fields() {
        let json = serde_json::to_string(&TimeStatusResponse { epoch: 1_719_403_899, synchronized: true }).unwrap();
        assert!(json.contains("epoch"));
        assert!(json.contains("synchronized"));
    }
}
