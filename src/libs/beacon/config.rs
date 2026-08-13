//! Configuration for the EYE BLE tag subsystem (loaded from `fiber.config.yaml`).

use serde::{Deserialize, Serialize};

use crate::libs::config::FieldThreshold;

/// Top-level EYE subsystem configuration.
///
/// `Default` is written out by hand rather than derived. A derived `Default`
/// ignores every `#[serde(default = "...")]` on the fields below, so
/// `BeaconConfig::default()` — which is what `config.beacon.clone().unwrap_or_default()`
/// hands the monitor when the YAML has no `eye:` section at all — produced a
/// struct with `publish_interval_s: 0`, `tag_timeout_s: 0` and
/// `scan_stall_secs: 0`. That is a broken configuration, not merely a disabled
/// one, and it would have started misbehaving the moment the subsystem was
/// switched on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeaconConfig {
    /// Enable the EYE BLE tag monitor.
    ///
    /// On by default: BLE tag support is a shipped feature of the product, and
    /// every unit that had it off carried the value from the shipped template
    /// rather than from a decision — there has never been a way to turn it off
    /// deliberately, so there was nothing to respect. `set_beacon_enabled` is that
    /// way; see the v2 -> v3 config migration for existing units.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// How often to publish the tag snapshot to MQTT, seconds.
    #[serde(default = "default_publish_interval_s")]
    pub publish_interval_s: u64,

    /// Mark a tag stale if not seen within this many seconds.
    #[serde(default = "default_tag_timeout_s")]
    pub tag_timeout_s: i64,

    /// Automatically provision a configured tag (apply the default profile) the
    /// first time it is seen advertising.
    #[serde(default)]
    pub auto_provision: bool,

    /// Master switch for the EN12830 temperature archive (white tags). When on,
    /// recording is auto-enabled at provisioning and gaps are back-filled from
    /// the tag's internal memory.
    #[serde(default = "default_true")]
    pub recording_enabled: bool,

    /// Default on-tag logging interval in minutes (tag supports 1 / 5 / 15).
    #[serde(default = "default_logging_interval_min")]
    pub default_logging_interval_min: u16,

    /// Fallback archive sync period in hours — download at least this often even
    /// without a detected gap.
    #[serde(default = "default_sync_fallback_hours")]
    pub sync_fallback_hours: u64,

    /// Self-heal a wedged BLE scan. On a combo Wi-Fi/BT controller the LE scan
    /// can stop delivering advertisements while BlueZ still reports
    /// `Discovering: yes`, and `StartDiscovery` can start timing out on D-Bus;
    /// neither surfaces as an error the monitor would otherwise see, so every tag
    /// simply goes stale forever. Off leaves the old behaviour: log and wait.
    #[serde(default = "default_true")]
    pub scan_stall_recovery: bool,

    /// Treat the scan as wedged after this many seconds with no advertisement
    /// from any audible tag. Must comfortably exceed the slowest tag's
    /// advertising interval — the PROXIMOS profile is 10 s, so the default is a
    /// wide margin over that rather than a tight bound.
    #[serde(default = "default_scan_stall_secs")]
    pub scan_stall_secs: u64,

    /// Report EYE tags seen advertising that are not in `tags` yet, so the viewer
    /// can offer them for adoption. Visibility only — an unregistered tag is
    /// published in the `eye/sensors` snapshot with `provisioning: "pending"` and
    /// is never written to `tags` by this flag alone. Registering one (manually,
    /// or by `auto_provision`) removes it from the discovered set; deleting it
    /// again makes it unknown, so it reappears.
    ///
    /// `Option` rather than a plain bool so an absent key round-trips as absent:
    /// a field missing from this struct is dropped when `BeaconConfig` is serialised
    /// back to `fiber.config.yaml`, which would strip an operator's setting
    /// irreversibly — rolling the binary back would not bring the key back,
    /// because the file was already overwritten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_discover: Option<bool>,

    /// Cap on how many unregistered tags to hold in the discovered set at once.
    /// A busy site can have far more tags in earshot than an operator wants to
    /// scroll, and each one costs a state entry and a slot in every snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_discover_max: Option<u32>,

    /// Bluetooth adapter for the EYE scan (e.g. "hci1"). `None` uses the default
    /// adapter. Lets the monitor bind a second controller so a co-located tag or
    /// simulator on another adapter is scannable (a controller can't scan its own
    /// advertisements) — used for on-device testing without a physical tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,

    /// Configured tags.
    #[serde(default)]
    pub tags: Vec<BeaconTagConfig>,
}

impl Default for BeaconConfig {
    /// Mirrors the `#[serde(default = "...")]` on each field, so a config with
    /// no `eye:` section behaves exactly like one that spells out the defaults.
    fn default() -> Self {
        Self {
            enabled: default_true(),
            publish_interval_s: default_publish_interval_s(),
            tag_timeout_s: default_tag_timeout_s(),
            auto_provision: false,
            recording_enabled: default_true(),
            default_logging_interval_min: default_logging_interval_min(),
            sync_fallback_hours: default_sync_fallback_hours(),
            scan_stall_recovery: default_true(),
            scan_stall_secs: default_scan_stall_secs(),
            auto_discover: None,
            auto_discover_max: None,
            adapter: None,
            tags: Vec::new(),
        }
    }
}

impl BeaconConfig {
    /// Effective logging interval (minutes) for a tag: per-tag override, else
    /// the subsystem default. Clamped to the tag-supported set {1, 5, 15}.
    pub fn interval_min_for(&self, tag: &BeaconTagConfig) -> u16 {
        let raw = tag
            .logging_interval_min
            .unwrap_or(self.default_logging_interval_min);
        match raw {
            1 => 1,
            15 => 15,
            _ => 5, // 5 is the default/kompromis; unknown values snap to it
        }
    }

    /// Whether the archive recording is active for a tag (per-tag override else
    /// the subsystem master switch).
    pub fn recording_on_for(&self, tag: &BeaconTagConfig) -> bool {
        self.recording_enabled && tag.recording.unwrap_or(true)
    }

    /// Report unregistered tags seen advertising. Off unless explicitly enabled.
    pub fn auto_discover_on(&self) -> bool {
        self.auto_discover.unwrap_or(false)
    }

    /// How many unregistered tags may be held at once. Zero is honoured as
    /// "none" rather than being treated as unset, so the cap can be used to turn
    /// the list off without clearing `auto_discover`.
    pub fn auto_discover_limit(&self) -> usize {
        self.auto_discover_max.unwrap_or(DEFAULT_AUTO_DISCOVER_MAX) as usize
    }

    /// Is this MAC already registered on this gateway? Case-insensitive.
    ///
    /// Drives both halves of discovery: a registered tag is never offered as a
    /// discovery candidate, and `auto_provision` only adopts a MAC for which this
    /// is false. Deleting a tag makes this false again, so it can be found anew.
    pub fn owns_tag(&self, mac: &str) -> bool {
        let up = mac.to_uppercase();
        self.tags.iter().any(|t| t.mac.to_uppercase() == up)
    }

    /// Insert or update a tag by MAC (case-insensitive; stored uppercased).
    /// Overwrites the name only when `name` is `Some`. Mirrors the YAML upsert in
    /// `ConfigApplier::update_beacon_tag_config` so the monitor's live view stays in
    /// sync with disk after an `add_eye_tag` command.
    ///
    /// Also switches the subsystem on. Adding a tag to a disabled subsystem is
    /// not a state anyone asks for: it is what produced units carrying
    /// `enabled: false` with `tags: [{enabled: true}]`, where the operator's
    /// tag was accepted and then never scanned for.
    pub fn upsert_tag(&mut self, mac: &str, name: Option<&str>) {
        self.enabled = true;
        let up = mac.to_uppercase();
        if let Some(t) = self.tags.iter_mut().find(|t| t.mac.to_uppercase() == up) {
            if let Some(n) = name {
                t.name = Some(n.to_string());
            }
        } else {
            self.tags.push(BeaconTagConfig {
                mac: up,
                name: name.map(|s| s.to_string()),
                enabled: true,
                logging_interval_min: None,
                recording: None,
                field_thresholds: Vec::new(),
                provisioned: None,
            });
        }
    }

    /// Remove a tag by MAC (case-insensitive). Returns whether one was removed.
    pub fn remove_tag(&mut self, mac: &str) -> bool {
        let up = mac.to_uppercase();
        let before = self.tags.len();
        self.tags.retain(|t| t.mac.to_uppercase() != up);
        self.tags.len() != before
    }

    /// Persist a tag's recording on/off + interval (from `set_eye_recording`).
    /// `interval_min == 0` means OFF: it sets `recording = Some(false)` so that
    /// `recording_on_for` returns false and the gap/fallback sync stops queueing
    /// downloads (which would otherwise re-`START_RECORD` the tag). Returns
    /// whether a matching tag was updated.
    pub fn set_recording(&mut self, mac: &str, interval_min: u16) -> bool {
        let up = mac.to_uppercase();
        if let Some(t) = self.tags.iter_mut().find(|t| t.mac.to_uppercase() == up) {
            t.recording = Some(interval_min != 0);
            if interval_min != 0 {
                t.logging_interval_min = Some(interval_min);
            }
            true
        } else {
            false
        }
    }

    /// Record that a tag's flash now has the PROXIMOS profile. Updates the live
    /// config so the next applier-driven YAML rewrite carries it, and so a
    /// re-read of the shared config inside the scan loop does not undo the
    /// in-memory `ProvisioningStatus`. Returns whether a matching tag was found.
    pub fn set_provisioned(&mut self, mac: &str, provisioned: bool) -> bool {
        let up = mac.to_uppercase();
        if let Some(t) = self.tags.iter_mut().find(|t| t.mac.to_uppercase() == up) {
            t.provisioned = Some(provisioned);
            true
        } else {
            false
        }
    }

    /// Upsert a per-field alarm threshold on a tag in the live config (so the
    /// scan loop's `evaluate_alarms` uses it without a restart). No-op if the
    /// tag isn't present (the applier persists to YAML either way).
    pub fn set_field_threshold(&mut self, mac: &str, t: FieldThreshold) {
        let up = mac.to_uppercase();
        if let Some(tag) = self.tags.iter_mut().find(|x| x.mac.to_uppercase() == up) {
            if let Some(existing) = tag
                .field_thresholds
                .iter_mut()
                .find(|ft| ft.field == t.field)
            {
                *existing = t;
            } else {
                tag.field_thresholds.push(t);
            }
        }
    }

    /// Remove a per-field alarm threshold from a tag in the live config.
    pub fn remove_field_threshold(&mut self, mac: &str, field: &str) {
        let up = mac.to_uppercase();
        if let Some(tag) = self.tags.iter_mut().find(|x| x.mac.to_uppercase() == up) {
            tag.field_thresholds.retain(|ft| ft.field != field);
        }
    }
}

/// A single configured EYE tag (identified by MAC).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeaconTagConfig {
    /// MAC address `AA:BB:CC:DD:EE:FF` (case-insensitive).
    pub mac: String,

    /// Operator-facing name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Whether this tag is active.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Per-tag on-tag logging interval in minutes (1 / 5 / 15). `None` inherits
    /// [`BeaconConfig::default_logging_interval_min`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging_interval_min: Option<u16>,

    /// Per-tag archive recording override. `None` inherits
    /// [`BeaconConfig::recording_enabled`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<bool>,

    /// Per-field alarm thresholds (fields: `temperature`, `humidity`), reusing
    /// the LoRaWAN sticker field-threshold model.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_thresholds: Vec<crate::libs::config::FieldThreshold>,

    /// Has this tag already had the PROXIMOS profile written to its flash?
    ///
    /// `ProvisioningStatus` is in-memory only, so without this every restart
    /// resets all tags to `PendingProvisioning` and — with `auto_provision` on —
    /// re-provisions the whole set at once. On a 16-tag gateway that burst
    /// contends with the scan for the adapter, which is exactly the failure this
    /// avoids. Writing to the tag's flash is idempotent but not free.
    ///
    /// Also the reason this field exists rather than being inferred: the applier
    /// serialises `BeaconConfig` back to `fiber.config.yaml` on every tag change, so
    /// a key the struct does not know about is silently dropped from the file.
    /// `None` means "never provisioned, or written by a build that predates this".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisioned: Option<bool>,
}

/// Default ceiling on the discovered-tag set when `auto_discover_max` is unset.
/// Well above a realistic room's worth of tags, low enough that a warehouse full
/// of them cannot grow the snapshot without bound.
pub const DEFAULT_AUTO_DISCOVER_MAX: u32 = 32;

fn default_publish_interval_s() -> u64 {
    30
}
fn default_tag_timeout_s() -> i64 {
    600
}
fn default_logging_interval_min() -> u16 {
    5
}
fn default_sync_fallback_hours() -> u64 {
    6
}
fn default_true() -> bool {
    true
}
fn default_scan_stall_secs() -> u64 {
    180
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Discovery decides what an operator is offered, so the gate that keeps a
    /// tag out of that list matters as much as the one that puts it in.
    #[test]
    fn auto_discover_is_off_unless_explicitly_enabled() {
        let mut c = BeaconConfig::default();
        assert!(
            !c.auto_discover_on(),
            "absent key must not enable discovery"
        );
        c.auto_discover = Some(false);
        assert!(!c.auto_discover_on());
        c.auto_discover = Some(true);
        assert!(c.auto_discover_on());
    }

    #[test]
    fn auto_discover_limit_defaults_but_honours_zero() {
        let mut c = BeaconConfig::default();
        assert_eq!(c.auto_discover_limit(), DEFAULT_AUTO_DISCOVER_MAX as usize);
        // Zero is a real answer ("show none"), not an unset value — otherwise the
        // cap could not be used to silence the list without also clearing the flag.
        c.auto_discover_max = Some(0);
        assert_eq!(c.auto_discover_limit(), 0);
        c.auto_discover_max = Some(4);
        assert_eq!(c.auto_discover_limit(), 4);
    }

    #[test]
    fn owns_tag_is_case_insensitive_and_drives_discovery_exclusion() {
        let mut c = BeaconConfig::default();
        c.upsert_tag("aa:bb:cc:dd:ee:01", Some("fridge"));
        // The scan uppercases MACs; the config may have been hand-edited in either
        // case. A mismatch here would offer an already-registered tag for adoption.
        assert!(c.owns_tag("AA:BB:CC:DD:EE:01"));
        assert!(c.owns_tag("aa:bb:cc:dd:ee:01"));
        assert!(!c.owns_tag("AA:BB:CC:DD:EE:02"));
    }

    #[test]
    fn a_removed_tag_becomes_discoverable_again() {
        // Explicitly required: deleting a tag must let it be found anew, so the
        // operator can re-add a tag they removed by mistake.
        let mut c = BeaconConfig::default();
        c.upsert_tag("AA:BB:CC:DD:EE:01", None);
        assert!(c.owns_tag("AA:BB:CC:DD:EE:01"));
        assert!(c.remove_tag("AA:BB:CC:DD:EE:01"));
        assert!(
            !c.owns_tag("AA:BB:CC:DD:EE:01"),
            "a deleted tag must be unknown again, or it can never be re-discovered"
        );
    }

    #[test]
    fn adopting_a_discovered_tag_is_a_plain_upsert() {
        // Auto-provision adopts by the same path an operator add uses, so the tag
        // it produces must be indistinguishable from a manually added one.
        let mut c = BeaconConfig::default();
        c.upsert_tag("AA:BB:CC:DD:EE:09", None);
        let t = c
            .tags
            .iter()
            .find(|t| t.mac == "AA:BB:CC:DD:EE:09")
            .unwrap();
        assert!(t.enabled, "an adopted tag must be scanned for");
        assert_eq!(
            t.provisioned, None,
            "provisioning is the monitor's to record"
        );
        assert!(t.field_thresholds.is_empty());
    }

    #[test]
    fn upsert_tag_inserts_uppercased_with_defaults() {
        let mut cfg = BeaconConfig::default();
        cfg.upsert_tag("aa:bb:cc:dd:ee:ff", Some("Freezer"));
        assert_eq!(cfg.tags.len(), 1);
        assert_eq!(cfg.tags[0].mac, "AA:BB:CC:DD:EE:FF");
        assert_eq!(cfg.tags[0].name.as_deref(), Some("Freezer"));
        assert!(cfg.tags[0].enabled);
    }

    #[test]
    fn upsert_tag_updates_in_place_and_keeps_name_when_none() {
        let mut cfg = BeaconConfig::default();
        cfg.upsert_tag("AA:BB:CC:DD:EE:FF", Some("Freezer"));
        // same MAC (lowercased) with a new name updates in place, no duplicate
        cfg.upsert_tag("aa:bb:cc:dd:ee:ff", Some("Fridge"));
        assert_eq!(cfg.tags.len(), 1, "must upsert, not duplicate");
        assert_eq!(cfg.tags[0].name.as_deref(), Some("Fridge"));
        // name None keeps the existing name
        cfg.upsert_tag("aa:bb:cc:dd:ee:ff", None);
        assert_eq!(cfg.tags[0].name.as_deref(), Some("Fridge"));
    }

    #[test]
    fn remove_tag_is_case_insensitive_and_reports() {
        let mut cfg = BeaconConfig::default();
        cfg.upsert_tag("AA:BB:CC:DD:EE:FF", None);
        assert!(cfg.remove_tag("aa:bb:cc:dd:ee:ff"));
        assert!(cfg.tags.is_empty());
        assert!(!cfg.remove_tag("AA:BB:CC:DD:EE:FF"), "absent -> false");
    }

    #[test]
    fn set_recording_off_makes_recording_on_for_false() {
        let mut cfg = BeaconConfig::default();
        cfg.recording_enabled = true;
        cfg.upsert_tag("AA:BB:CC:DD:EE:FF", Some("Freezer"));
        // interval 5 -> on
        assert!(cfg.set_recording("aa:bb:cc:dd:ee:ff", 5));
        let tag = cfg.tags[0].clone();
        assert_eq!(tag.recording, Some(true));
        assert_eq!(tag.logging_interval_min, Some(5));
        assert!(cfg.recording_on_for(&tag));
        // interval 0 -> off, and recording_on_for must be false (H1)
        assert!(cfg.set_recording("AA:BB:CC:DD:EE:FF", 0));
        let tag = cfg.tags[0].clone();
        assert_eq!(tag.recording, Some(false));
        assert!(
            !cfg.recording_on_for(&tag),
            "interval 0 must turn recording off"
        );
        // unknown MAC -> false
        assert!(!cfg.set_recording("11:22:33:44:55:66", 1));
    }
}
