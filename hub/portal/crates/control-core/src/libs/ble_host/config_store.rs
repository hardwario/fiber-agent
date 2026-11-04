//! `ConfigStore` over the agent's own `ConfigApplier`.
//!
//! The crate asks for "read the `eye:` section" and "change it atomically". It deliberately does
//! not know about YAML, backups, `.backups/`, live reload or the audit table — those are the
//! agent's, and routing through `ConfigApplier` is what keeps a BLE-originated change
//! indistinguishable from an MQTT-originated one at the store layer (AC 4).
//!
//! **Why a diff and not a write.** `ConfigStore::mutate` hands us a closure over a whole
//! `EyeSection`. The applier's API is per-change (`apply_beacon_tag_config`, `apply_beacon_enabled`,
//! …), each with its own backup + audit + reload. So `mutate` loads, runs the closure on a copy,
//! and works out which applier calls that mutation *was* — which both preserves v1's trail and
//! refuses to invent a new write path next to it.
//!
//! **The add-and-enable case is one write** (AC 4). Enrolling the first beacon on a Hub with
//! `eye.enabled: false` is two edits — append the tag, flip the flag — and two `write_config`
//! calls would leave a window where the tag exists and the subsystem is off, plus two audit rows
//! for one operator action. `ConfigApplier::apply_beacon_tag_config_and_enable` does both inside
//! one write; this module is its only caller.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use control_ble::config::{EyeSection, EyeTag};
use control_ble::error::{Component, Error, Kind, Result};
use control_ble::host::{AuditEntry, ConfigStore};

use crate::libs::config_applier::ConfigApplier;

pub struct ApplierConfigStore {
    applier: Arc<ConfigApplier>,
    path: PathBuf,
}

impl ApplierConfigStore {
    /// `config_dir` is the applier's directory; the file is its `fiber.config.yaml`.
    pub fn new(applier: Arc<ConfigApplier>, config_dir: &Path) -> ApplierConfigStore {
        ApplierConfigStore {
            applier,
            path: config_dir.join("fiber.config.yaml"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read_section(&self) -> Result<EyeSection> {
        let text = std::fs::read_to_string(&self.path).map_err(|e| {
            Error::new(
                Kind::StorageError,
                None,
                Component::DeviceConfig,
                format!("cannot read {}: {e}", self.path.display()),
            )
        })?;
        section_from_yaml(&text)
    }

    /// One applier call per difference. Order matters: the tag has to exist before its recording
    /// interval can be set, and `enabled` is flipped with the add when both changed (AC 4).
    fn apply_diff(&self, before: &EyeSection, after: &EyeSection) -> Result<()> {
        let added: Vec<&EyeTag> = after
            .tags
            .iter()
            .filter(|t| !before.tags.iter().any(|b| eq_mac(&b.mac, &t.mac)))
            .collect();
        let removed: Vec<&EyeTag> = before
            .tags
            .iter()
            .filter(|b| !after.tags.iter().any(|t| eq_mac(&t.mac, &b.mac)))
            .collect();
        let enable_flip = before.enabled != after.enabled;

        // The AC 4 case: exactly one tag appeared and the flag went false -> true in the same
        // mutation. Anything more complicated is not the enrolment path and takes the plain route.
        if enable_flip && after.enabled && added.len() == 1 && removed.is_empty() {
            let tag = added[0];
            let r = self
                .applier
                .apply_beacon_tag_config_and_enable(tag.mac.clone(), tag.name.clone());
            check(r, "add-and-enable")?;
        } else {
            for tag in &added {
                check(
                    self.applier
                        .apply_beacon_tag_config(tag.mac.clone(), tag.name.clone()),
                    "tag add",
                )?;
            }
            if enable_flip {
                check(
                    self.applier.apply_beacon_enabled(after.enabled),
                    "enabled flip",
                )?;
            }
        }

        for tag in &removed {
            check(
                self.applier.remove_beacon_tag_config(tag.mac.clone()),
                "tag remove",
            )?;
        }

        // Renames and per-tag settings on tags that already existed.
        for tag in &after.tags {
            let Some(old) = before.tags.iter().find(|b| eq_mac(&b.mac, &tag.mac)) else {
                continue;
            };
            if old.name != tag.name {
                check(
                    self.applier
                        .apply_beacon_tag_config(tag.mac.clone(), tag.name.clone()),
                    "tag rename",
                )?;
            }
            if old.recording != tag.recording
                || old.logging_interval_min != tag.logging_interval_min
            {
                // v1's applier models "recording" as an interval: 0 stops it. A `recording: false`
                // with an interval set is the stop case, which is why the flag is read first.
                let interval = match (tag.recording, tag.logging_interval_min) {
                    (Some(false), _) => 0,
                    (_, Some(m)) => m,
                    (Some(true), None) => DEFAULT_LOGGING_INTERVAL_MIN,
                    (None, None) => continue,
                };
                check(
                    self.applier
                        .apply_beacon_recording(tag.mac.clone(), interval),
                    "recording",
                )?;
            }
        }
        Ok(())
    }
}

/// What `recording: true` means when no interval was given — v1's own default (`applier.rs`).
pub const DEFAULT_LOGGING_INTERVAL_MIN: u16 = 15;

fn eq_mac(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn check(r: crate::libs::config_applier::ApplyResult, what: &str) -> Result<()> {
    if r.success {
        return Ok(());
    }
    Err(Error::new(
        Kind::StorageError,
        None,
        Component::DeviceConfig,
        format!(
            "{what} was refused: {}",
            r.error_message.unwrap_or_else(|| "no reason given".into())
        ),
    ))
}

/// Pull the `eye:` mapping out of a whole device config and parse it as the crate's section.
///
/// A missing `eye:` is a default section, not an error: a Hub that has never enrolled a beacon has
/// no reason to carry the key, and refusing to start there would be a regression from v1.
pub fn section_from_yaml(text: &str) -> Result<EyeSection> {
    let doc: serde_yaml::Value = serde_yaml::from_str(text).map_err(|e| {
        Error::new(
            Kind::InvalidArgument,
            None,
            Component::DeviceConfig,
            format!("the device config is not valid YAML: {e}"),
        )
    })?;
    let Some(eye) = doc.get("eye") else {
        return Ok(EyeSection::default());
    };
    serde_yaml::from_value(eye.clone()).map_err(|e| {
        Error::new(
            Kind::InvalidArgument,
            None,
            Component::DeviceConfig,
            format!("the eye: section is invalid: {e}"),
        )
    })
}

/// The enrolled tags an already-parsed `eye:` value names, for
/// `config/state.eye_tags` (devices.backend.md §22.2).
///
/// **`None` is not `[]`.** The portal reads this key as the authoritative
/// enrolled inventory: `Some([])` means "this Hub enrols nothing, prune every
/// registered Beacon you hold", and `None` means "this document makes no
/// statement". So the answer is `Some` only when the `eye:` section actually
/// parsed:
///
/// * absent / null → `Some([])`. A Hub that has never enrolled a Beacon has no
///   reason to carry the key (see `section_from_yaml`), and that is a genuine
///   empty set, not an unknown one.
/// * present and parseable → `Some(tags)`.
/// * present and malformed → `None`. We do not know the enrolled set, and
///   saying "none" would empty the portal's fleet on a typo.
///
/// `EyeTag` carries no `location`, so the entries the caller builds carry
/// none either — the wire key is omitted rather than sent as an invented null.
pub fn enrolled_tags(eye: &serde_yaml::Value) -> Option<Vec<EyeTag>> {
    if eye.is_null() {
        return Some(Vec::new());
    }
    match serde_yaml::from_value::<EyeSection>(eye.clone()) {
        Ok(section) => Some(section.tags),
        Err(e) => {
            eprintln!(
                "[ble_host::config_store] the eye: section did not parse ({e}); config/state will omit eye_tags rather than claim an empty enrolled set"
            );
            None
        }
    }
}

impl ConfigStore for ApplierConfigStore {
    fn load(&self) -> Result<EyeSection> {
        self.read_section()
    }

    fn mutate(&self, f: &mut dyn FnMut(&mut EyeSection)) -> Result<EyeSection> {
        let before = self.read_section()?;
        let mut after = before.clone();
        f(&mut after);
        if after == before {
            // A no-op mutation writes nothing: no backup, no audit row, no reload. The crate relies
            // on this to make "add a beacon that is already enrolled" idempotent.
            return Ok(before);
        }
        self.apply_diff(&before, &after)?;
        self.read_section()
    }

    fn audit(&self, entry: &AuditEntry) {
        // One row per operator action, in the v2 vocabulary (`beacon-add`, …), with the origin that
        // distinguishes the three mutation surfaces. Serialising the whole entry keeps the detail
        // machine-readable for the Class IIa trail.
        let detail = serde_json::to_string(entry).unwrap_or_else(|_| entry.action.to_string());
        self.applier.log_audit_public(entry.action, detail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use control_ble::host::Origin;

    fn applier_in(dir: &Path) -> Arc<ConfigApplier> {
        Arc::new(ConfigApplier::new(dir).expect("applier"))
    }

    fn write_config(dir: &Path, body: &str) {
        std::fs::write(dir.join("fiber.config.yaml"), body).expect("seed");
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let base = std::env::temp_dir().join(format!(
                "ble-host-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&base).expect("temp dir");
            TempDir(base)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `ConfigApplier::new` creates `.backups/` eagerly, so presence proves nothing — only the
    /// number of files in it does.
    fn backup_count(dir: &Path) -> usize {
        std::fs::read_dir(dir.join(".backups"))
            .map(|d| d.flatten().count())
            .unwrap_or(0)
    }

    const SEED: &str = "system:\n  device_label: bench\neye:\n  enabled: false\n  tags: []\n";

    #[test]
    fn config_store_upserts_into_eye_tags_through_the_applier() {
        let dir = TempDir::new("upsert");
        write_config(&dir.0, SEED);
        let store = ApplierConfigStore::new(applier_in(&dir.0), &dir.0);

        let after = store
            .mutate(&mut |s| {
                s.tags.push(EyeTag {
                    mac: "AA:BB:CC:DD:EE:FF".into(),
                    name: Some("freezer".into()),
                    enabled: true,
                    logging_interval_min: None,
                    recording: None,
                    field_thresholds: Vec::new(),
                    provisioned: None,
                });
            })
            .expect("mutate");

        assert_eq!(after.tags.len(), 1);
        assert_eq!(after.tags[0].mac, "AA:BB:CC:DD:EE:FF");
        assert_eq!(after.tags[0].name.as_deref(), Some("freezer"));

        // And it really is on disk, through the applier's own writer.
        let text = std::fs::read_to_string(dir.0.join("fiber.config.yaml")).expect("read");
        assert!(text.contains("AA:BB:CC:DD:EE:FF"), "{text}");
        assert_eq!(
            backup_count(&dir.0),
            1,
            "the applier's backup discipline still applies"
        );
    }

    #[test]
    fn a_no_op_mutation_writes_nothing() {
        let dir = TempDir::new("noop");
        write_config(&dir.0, SEED);
        let store = ApplierConfigStore::new(applier_in(&dir.0), &dir.0);
        let before = std::fs::read_to_string(dir.0.join("fiber.config.yaml")).expect("read");
        let after = store.mutate(&mut |_| {}).expect("mutate");
        assert!(after.tags.is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.0.join("fiber.config.yaml")).expect("read"),
            before,
            "an unchanged section must not rewrite the file"
        );
        assert_eq!(
            backup_count(&dir.0),
            0,
            "and must not leave a backup behind"
        );
    }

    #[test]
    fn adding_the_first_beacon_and_enabling_is_one_write() {
        // AC 4. Two writes would leave a window where the tag exists and the subsystem is off.
        let dir = TempDir::new("addenable");
        write_config(&dir.0, SEED);
        let store = ApplierConfigStore::new(applier_in(&dir.0), &dir.0);
        store
            .mutate(&mut |s| {
                s.enabled = true;
                s.tags.push(EyeTag {
                    mac: "AA:BB:CC:DD:EE:FF".into(),
                    name: None,
                    enabled: true,
                    logging_interval_min: None,
                    recording: None,
                    field_thresholds: Vec::new(),
                    provisioned: None,
                });
            })
            .expect("mutate");

        let after = store.load().expect("load");
        assert!(after.enabled, "the flag flipped");
        assert_eq!(after.tags.len(), 1, "and the tag landed");

        assert_eq!(backup_count(&dir.0), 1, "one write, therefore one backup");
    }

    #[test]
    fn removing_a_tag_goes_through_the_appliers_remove_path() {
        let dir = TempDir::new("remove");
        write_config(
            &dir.0,
            "eye:\n  enabled: true\n  tags:\n    - mac: AA:BB:CC:DD:EE:FF\n      name: freezer\n",
        );
        let store = ApplierConfigStore::new(applier_in(&dir.0), &dir.0);
        let after = store
            .mutate(&mut |s| s.tags.retain(|t| t.mac != "AA:BB:CC:DD:EE:FF"))
            .expect("mutate");
        assert!(after.tags.is_empty());
        let text = std::fs::read_to_string(dir.0.join("fiber.config.yaml")).expect("read");
        assert!(!text.contains("AA:BB:CC:DD:EE:FF"), "{text}");
    }

    #[test]
    fn load_tolerates_a_config_with_no_eye_section() {
        let dir = TempDir::new("noeye");
        write_config(&dir.0, "system:\n  device_label: bench\n");
        let store = ApplierConfigStore::new(applier_in(&dir.0), &dir.0);
        let s = store.load().expect("a default section, not an error");
        assert!(s.tags.is_empty());
        assert!(s.enabled, "the crate's default");
    }

    #[test]
    fn load_reports_an_invalid_eye_section_rather_than_defaulting() {
        let dir = TempDir::new("badeye");
        write_config(&dir.0, "eye:\n  enabled: \"yes please\"\n");
        let store = ApplierConfigStore::new(applier_in(&dir.0), &dir.0);
        let err = store.load().expect_err("refused");
        assert_eq!(err.kind(), Kind::InvalidArgument);
    }

    #[test]
    fn legacy_transport_keys_survive_a_round_trip() {
        // v1 configs carry `publish_interval_s` / `tag_timeout_s`. The crate ignores them; the
        // store must not delete them behind an operator's back.
        let dir = TempDir::new("legacy");
        write_config(
            &dir.0,
            "eye:\n  enabled: true\n  publish_interval_s: 30\n  tag_timeout_s: 600\n  tags: []\n",
        );
        let store = ApplierConfigStore::new(applier_in(&dir.0), &dir.0);
        let s = store.load().expect("load");
        assert!(
            s.legacy.contains_key("publish_interval_s"),
            "{:?}",
            s.legacy
        );
        store
            .mutate(&mut |s| {
                s.tags.push(EyeTag {
                    mac: "AA:BB:CC:DD:EE:FF".into(),
                    name: None,
                    enabled: true,
                    logging_interval_min: None,
                    recording: None,
                    field_thresholds: Vec::new(),
                    provisioned: None,
                })
            })
            .expect("mutate");
        let text = std::fs::read_to_string(dir.0.join("fiber.config.yaml")).expect("read");
        assert!(text.contains("publish_interval_s"), "{text}");
    }

    #[test]
    fn an_audit_entry_is_serialised_whole() {
        // No storage handle here, so the row goes nowhere — the assertion is that it does not
        // panic and that the detail is the JSON a reviewer would want.
        let dir = TempDir::new("audit");
        write_config(&dir.0, SEED);
        let store = ApplierConfigStore::new(applier_in(&dir.0), &dir.0);
        let entry = AuditEntry {
            action: "beacon-add",
            mac: Some("AA:BB:CC:DD:EE:FF".into()),
            origin: Origin::SignedMqtt,
            detail: "enabled_flipped".into(),
        };
        store.audit(&entry);
        let json = serde_json::to_string(&entry).expect("serialize");
        assert!(json.contains("beacon-add"), "{json}");
        assert!(json.contains("signed_mqtt"), "{json}");
    }

    /// `config/state.eye_tags` prunes the portal's registered fleet, so the
    /// three answers this function can give must stay distinguishable
    /// (devices.backend.md §22.2).
    #[test]
    fn the_enrolled_set_is_none_only_when_the_section_will_not_parse() {
        let eye: serde_yaml::Value =
            serde_yaml::from_str("tags:\n  - mac: EE:E0:00:00:00:01\n    name: Freezer\n")
                .expect("yaml");
        let tags = enrolled_tags(&eye).expect("a parseable section is a known set");
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].mac, "EE:E0:00:00:00:01");
        assert_eq!(tags[0].name.as_deref(), Some("Freezer"));

        // No `eye:` at all is a Hub that has never enrolled anything — a real
        // empty set, the same reading `section_from_yaml` takes.
        assert_eq!(
            enrolled_tags(&serde_yaml::Value::Null).map(|t| t.len()),
            Some(0)
        );
        // An `eye:` section with no `tags:` key, likewise.
        let flags: serde_yaml::Value = serde_yaml::from_str("enabled: true\n").expect("yaml");
        assert_eq!(enrolled_tags(&flags).map(|t| t.len()), Some(0));

        // Malformed: we do not know the enrolled set, and `Some(vec![])` here
        // would tell the portal to prune every registered Beacon it holds.
        let broken: serde_yaml::Value =
            serde_yaml::from_str("tags: not-a-list\n").expect("yaml is valid, the schema is not");
        assert!(enrolled_tags(&broken).is_none());
    }
}
