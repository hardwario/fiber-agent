//! The Hub's own hardware identity, as its device tree reports it — published in
//! `system/info.hardware { model, revision, serial }` (plan/portal/devices.hub.md §3.1,
//! amendment 2026-09-28).
//!
//! Two properties, both written by the Raspberry Pi firmware and carried into the slot's
//! device tree by U-Boot (`boot.cmd.in` copies `serial-number` across):
//!
//! - `serial-number` — the full CPU serial, e.g. `100000004e970f4a`. The hostname carries
//!   only its last eight characters.
//! - `model` — the compute module's name with its revision, e.g.
//!   "Raspberry Pi Compute Module 4 Rev 1.1", split here into model and revision.
//!
//! There is no HARDWARIO product EEPROM on the board, so the Hub board's own revision is not
//! knowable from here; `revision` is the compute module's. Nothing is invented: a property
//! that cannot be read is simply absent from the payload, and the portal shows "—".
//!
//! Read once per process. The device tree does not change between boots, so re-reading it on
//! every `system/info` would only repeat the same answer — including the same "absent" on a
//! dev host with no `/proc/device-tree`.

use std::path::Path;
use std::sync::OnceLock;

use serde::Serialize;

/// Where the running kernel exposes its device tree.
pub const DEVICE_TREE_ROOT: &str = "/proc/device-tree";

/// What the device tree says about this unit. Every field is optional on its own and is
/// omitted from the JSON when `None`, which is the wire contract: absent, never `null`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HardwareInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

impl HardwareInfo {
    /// The `system/info.hardware` object, or `None` when nothing could be read — the whole
    /// key is then left out rather than published as an empty object.
    pub fn to_json(&self) -> Option<serde_json::Value> {
        if self.model.is_none() && self.revision.is_none() && self.serial.is_none() {
            return None;
        }
        serde_json::to_value(self).ok()
    }
}

/// A device-tree string property's value: the first NUL-terminated string, trimmed, or
/// `None` when that is empty or not UTF-8.
///
/// The kernel exposes string properties with their terminating NUL (and a string-list
/// property as several NUL-separated strings), so the raw bytes are never usable as-is.
pub fn parse_dt_string(raw: &[u8]) -> Option<String> {
    let first = raw.split(|b| *b == 0).next().unwrap_or_default();
    let text = std::str::from_utf8(first).ok()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Splits "Raspberry Pi Compute Module 4 Rev 1.1" into the model and the revision `1.1`.
///
/// The last " Rev " is the separator, as the firmware formats it. A string without one is
/// the model whole, with no revision, rather than a guess at where one might be.
pub fn split_model(model: &str) -> (String, Option<String>) {
    let model = model.trim();
    match model.rsplit_once(" Rev ") {
        Some((name, revision)) if !name.trim().is_empty() && !revision.trim().is_empty() => {
            (name.trim().to_string(), Some(revision.trim().to_string()))
        }
        _ => (model.to_string(), None),
    }
}

/// [`HardwareInfo`] from the device tree rooted at `root` — [`DEVICE_TREE_ROOT`] on a Hub, a
/// fixture directory in a test. A missing or unreadable property leaves its fields `None`.
pub fn read_from(root: &Path) -> HardwareInfo {
    let property = |name: &str| {
        std::fs::read(root.join(name))
            .ok()
            .and_then(|raw| parse_dt_string(&raw))
    };
    let (model, revision) = match property("model") {
        Some(raw) => {
            let (model, revision) = split_model(&raw);
            (Some(model), revision)
        }
        None => (None, None),
    };
    HardwareInfo {
        model,
        revision,
        serial: property("serial-number"),
    }
}

/// This unit's [`HardwareInfo`], read from [`DEVICE_TREE_ROOT`] on first use.
pub fn current() -> &'static HardwareInfo {
    static CURRENT: OnceLock<HardwareInfo> = OnceLock::new();
    CURRENT.get_or_init(|| read_from(Path::new(DEVICE_TREE_ROOT)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device-tree directory holding the given properties, bytes as the kernel serves them.
    fn device_tree(properties: &[(&str, &[u8])]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, raw) in properties {
            std::fs::write(dir.path().join(name), raw).expect("write property");
        }
        dir
    }

    #[test]
    fn a_string_property_loses_its_nul_terminator() {
        assert_eq!(
            parse_dt_string(b"100000004e970f4a\0").as_deref(),
            Some("100000004e970f4a")
        );
        // Without one too, and only the first string of a string list.
        assert_eq!(parse_dt_string(b"abc").as_deref(), Some("abc"));
        assert_eq!(
            parse_dt_string(b"first\0second\0").as_deref(),
            Some("first")
        );
    }

    #[test]
    fn an_empty_or_non_utf8_property_is_absent() {
        assert_eq!(parse_dt_string(b""), None);
        assert_eq!(parse_dt_string(b"\0"), None);
        assert_eq!(parse_dt_string(b"  \n\0"), None);
        assert_eq!(parse_dt_string(b"\xff\xfe\0"), None);
    }

    #[test]
    fn the_model_splits_at_rev() {
        assert_eq!(
            split_model("Raspberry Pi Compute Module 4 Rev 1.1"),
            (
                "Raspberry Pi Compute Module 4".to_string(),
                Some("1.1".to_string())
            )
        );
        assert_eq!(
            split_model("Raspberry Pi 4 Model B Rev 1.4"),
            (
                "Raspberry Pi 4 Model B".to_string(),
                Some("1.4".to_string())
            )
        );
    }

    #[test]
    fn a_model_without_rev_is_the_model_whole() {
        assert_eq!(
            split_model("Raspberry Pi Compute Module 4"),
            ("Raspberry Pi Compute Module 4".to_string(), None)
        );
        // A dangling separator is not a revision.
        assert_eq!(
            split_model("Raspberry Pi Compute Module 4 Rev "),
            ("Raspberry Pi Compute Module 4 Rev".to_string(), None)
        );
    }

    #[test]
    fn a_cm4_device_tree_reads_whole() {
        let dir = device_tree(&[
            ("model", b"Raspberry Pi Compute Module 4 Rev 1.1\0"),
            ("serial-number", b"100000004e970f4a\0"),
        ]);
        let info = read_from(dir.path());
        assert_eq!(
            info,
            HardwareInfo {
                model: Some("Raspberry Pi Compute Module 4".to_string()),
                revision: Some("1.1".to_string()),
                serial: Some("100000004e970f4a".to_string()),
            }
        );
        assert_eq!(
            info.to_json(),
            Some(serde_json::json!({
                "model": "Raspberry Pi Compute Module 4",
                "revision": "1.1",
                "serial": "100000004e970f4a",
            }))
        );
    }

    /// A missing property leaves its fields out of the JSON, not `null`.
    #[test]
    fn a_missing_property_is_left_out() {
        let dir = device_tree(&[("serial-number", b"100000004e970f4a\0")]);
        let info = read_from(dir.path());
        assert_eq!(info.model, None);
        assert_eq!(info.revision, None);
        assert_eq!(
            info.to_json(),
            Some(serde_json::json!({ "serial": "100000004e970f4a" }))
        );

        let dir = device_tree(&[("model", b"Raspberry Pi Compute Module 4\0")]);
        assert_eq!(
            read_from(dir.path()).to_json(),
            Some(serde_json::json!({ "model": "Raspberry Pi Compute Module 4" }))
        );
    }

    /// No device tree at all — a dev host — publishes no `hardware` key.
    #[test]
    fn no_device_tree_publishes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let info = read_from(&dir.path().join("no-such-device-tree"));
        assert_eq!(info, HardwareInfo::default());
        assert_eq!(info.to_json(), None);
    }
}
