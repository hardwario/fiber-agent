//! `DeviceInfo` — FB07's read and FB0A's label write.
//!
//! **Uptime is read, not shelled.** v1 ran `uptime -p` and put its stdout on FB07. Decision #6
//! replaces the trivial read-only shell-outs, and this is one: `/proc/uptime` is two floats, the
//! first of which is what `uptime` itself reads. Dropping the exec removes a process spawn from a
//! characteristic a phone reads before it has authenticated — the one surface reachable without a
//! session — and takes `procps` off the BLE path's dependency list.
//!
//! **Version comes from the build, not the filesystem.** `FIBER_VERSION` is set by Yocto so an
//! image can be identified from a phone; `CARGO_PKG_VERSION` is the developer fallback. Reading it
//! from a file would let a `/data` edit misreport which image is running, which is exactly the
//! claim a service engineer needs to trust.

use std::sync::Arc;

use control_ble::host::DeviceInfo;

use crate::libs::config_applier::ConfigApplier;

pub struct AgentDeviceInfo {
    hostname: String,
    mac: String,
    applier: Arc<ConfigApplier>,
    config_path: std::path::PathBuf,
}

impl AgentDeviceInfo {
    pub fn new(
        hostname: String,
        mac: String,
        applier: Arc<ConfigApplier>,
        config_dir: &std::path::Path,
    ) -> AgentDeviceInfo {
        AgentDeviceInfo {
            hostname,
            mac,
            applier,
            config_path: config_dir.join("fiber.config.yaml"),
        }
    }

    /// The adapter address as the agent knows it. FB07 publishes the *live* controller address when
    /// it has one; this is the boot-time value and the fallback.
    pub fn mac(&self) -> &str {
        &self.mac
    }
}

/// `FIBER_VERSION` at build time, else the crate version — exactly v1's FB07 rule.
pub fn firmware_version() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|raw| os_release_version(&raw))
        .unwrap_or_else(build_version)
}

/// The version baked in at build time: `FIBER_VERSION` when set, else the crate version.
pub fn build_version() -> String {
    option_env!("FIBER_VERSION")
        .filter(|v| !v.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
        .to_string()
}

/// The Hub image version from an `os-release` text: `VERSION_ID`, else `VERSION`.
pub fn os_release_version(raw: &str) -> Option<String> {
    let field = |key: &str| {
        raw.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_string())
            .filter(|v| !v.is_empty())
    };
    field("VERSION_ID").or_else(|| field("VERSION"))
}

/// Seconds since boot, formatted the way an operator reads it.
///
/// `/proc/uptime`'s first field. An unreadable or malformed file yields `"unknown"` rather than a
/// zero: "0m" would claim the Hub had just rebooted, which is a different and much more alarming
/// statement than "we could not tell".
pub fn uptime_string() -> String {
    let Ok(text) = std::fs::read_to_string("/proc/uptime") else {
        return "unknown".to_string();
    };
    let Some(secs) = text
        .split_whitespace()
        .next()
        .and_then(|f| f.parse::<f64>().ok())
    else {
        return "unknown".to_string();
    };
    format_uptime(secs as u64)
}

pub fn format_uptime(total_secs: u64) -> String {
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h {minutes}m"),
    }
}

impl DeviceInfo for AgentDeviceInfo {
    fn hostname(&self) -> String {
        self.hostname.clone()
    }

    fn version(&self) -> String {
        firmware_version()
    }

    fn uptime(&self) -> String {
        uptime_string()
    }

    fn label(&self) -> String {
        // Read through the file rather than a cached `Config`: the label can be changed by MQTT or
        // the control socket while the process runs, and FB0A must not show a stale one.
        std::fs::read_to_string(&self.config_path)
            .ok()
            .and_then(|t| serde_yaml::from_str::<serde_yaml::Value>(&t).ok())
            .and_then(|v| {
                v.get("system")
                    .and_then(|s| s.get("device_label"))
                    .and_then(|l| l.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default()
    }

    fn set_label(&self, label: &str) -> Result<(), String> {
        let r = self.applier.apply_device_label_change(label.to_string());
        if r.success {
            Ok(())
        } else {
            Err(r
                .error_message
                .unwrap_or_else(|| "the label change was refused".to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_is_read_from_proc_and_never_claims_a_fresh_boot_when_unknown() {
        let s = uptime_string();
        assert_ne!(s, "0m", "a real machine has been up longer than a minute");
        assert!(!s.is_empty());
    }

    #[test]
    fn the_uptime_format_matches_what_an_operator_expects() {
        assert_eq!(format_uptime(0), "0m");
        assert_eq!(format_uptime(59), "0m");
        assert_eq!(format_uptime(90), "1m");
        assert_eq!(format_uptime(3_600), "1h 0m");
        assert_eq!(format_uptime(3_661), "1h 1m");
        assert_eq!(format_uptime(90_061), "1d 1h 1m");
    }

    #[test]
    fn the_version_is_the_images_then_the_builds() {
        let raw =
            "ID=proximos\nVERSION=\"v1.0.0-rc1\"\nVERSION_ID=v1.0.0-rc1-f8a82b31-2-g1df95f8f\n";
        assert_eq!(
            os_release_version(raw).as_deref(),
            Some("v1.0.0-rc1-f8a82b31-2-g1df95f8f")
        );
        assert_eq!(
            os_release_version("VERSION=\"v1.0.0\"\n").as_deref(),
            Some("v1.0.0")
        );
        assert_eq!(os_release_version("ID=debian\nVERSION_ID=\n"), None);
        assert!(!build_version().is_empty());
        assert!(!firmware_version().is_empty());
    }
}
